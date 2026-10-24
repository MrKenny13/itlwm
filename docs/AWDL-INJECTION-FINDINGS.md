# AWDL injection on AX211 — findings and roadmap

État au 2026-10-24, après 10 runs terrain (bindings 0..9, kext v2.3 → v2.8.4).

## But

Faire marcher l'injection de trames AWDL (pour AirDrop) depuis le kext
`AirportItlwm` (portage macOS d'iwx/OpenBSD) sur une carte Intel **AX211**
(`8086:7a70`, device-family AX210, gen3, firmware `so-a0-gf4-a0-68`,
uCode major 0x44). La réception et l'ACK du pair marchent ; **l'émission
tue le firmware**.

## Diagnostic confirmé

**Ce firmware est piloté, côté Linux, par la nouvelle API `iwlmld`.
itlwm parle l'ancienne API `iwlmvm`.** L'ancienne suffit pour la
réception, l'ACK et le Wi-Fi STA normal, mais ce firmware ne programme
pas proprement une file d'émission d'injection (contexte monitor/PIBSS)
montée à l'ancienne — d'où l'assert systématique au premier kick TX.

### Preuves

1. **Contrôle sans émission (v2.8.2, binding 7 + `--sans-emission`)** :
   session complète montée (contexte PIBSS + station + file TVQM), 2 min,
   `fw_err=0`, `rx` qui monte. → **monter la session est sain ; c'est le
   kick TX qui tue.**

2. **La commande TX est identique au bit près** à `iwx_tx` natif gen3
   (len, offload_assist, header recopié, flags, rate_n_flags, TB0/1/2,
   byte-count table, kick WRPTR). Le chemin natif marche pour le Wi-Fi
   normal. → **ni la trame ni le kick ne sont en cause.**

3. **Tous les types de station échouent à l'identique** (bindings 0..6 :
   monitor/GP, LINK, aux, mcast, bcast_sta), `done=0` partout — le
   firmware ne complète jamais une trame : elle entre dans l'anneau
   (`queued=1`) puis LMAC `ADVANCED_SYSASSERT` (`il1=0x4CF022`). →
   **la station n'est pas l'axe.**

4. **Trace Linux `trace-cmd` (owly + iwlwifi, même carte, RX réussi)** :
   - owly injecte sur **une seule file TVQM : `0x1c` (28)**.
   - owly n'émet **aucune** commande ancienne API pour le contexte/la
     station : pas de `MAC_CONTEXT_CMD` (0x28), pas d'`ADD_STA` (0x18),
     pas de `SCD_QUEUE_CFG` (0x1d).
   - il utilise la **nouvelle API** :

     | commande | id | rôle |
     |---|---|---|
     | `MAC_CONFIG_CMD` | `0x03.0x08` | contexte MAC |
     | `LINK_CONFIG_CMD` | `0x03.0x09` | lien |
     | `STA_CONFIG_CMD` | `0x03.0x0a` | station |
     | `SCD_QUEUE_CONFIG_CMD` | `0x05.0x17` | file (data-path) |
     | `TLC_MNG_CONFIG_CMD` | `0x05.0x0f` | rate-scaling |
     | `SESSION_PROTECTION_CMD` | `0x03.0x05` | protection |

     (itlwm envoie, lui, `0x01.0x1d` pour la file et `0x01.0x18`/`0x28`
     pour station/contexte.)

5. **Binding 9 (v2.8.4)** : on a remplacé **seulement** la commande de
   file par la nouvelle `SCD_QUEUE_CONFIG_CMD` (0x05.0x17), en gardant
   la station ancienne API. Résultat : assert immédiat (`fw_err=61`),
   avec **`umac d1=0x17 d2=0x5`** dans le dump — le firmware assert **en
   nommant notre commande 0x17/groupe 0x5**. → la nouvelle commande de
   file est bien reçue, mais son `sta_mask = BIT(0)` référence une
   station absente de la table nouvelle API. **On ne peut pas mélanger
   les dialectes.**

## Conclusion

Le correctif = porter toute la chaîne d'injection vers l'API `iwlmld` :
`MAC_CONFIG_CMD` → `LINK_CONFIG_CMD` → `STA_CONFIG_CMD` →
`SCD_QUEUE_CONFIG_CMD` (+ `TLC_MNG_CONFIG_CMD`). itlwm n'a aujourd'hui
**aucune** de ces commandes (sauf TLC). C'est un vrai chantier de
sous-système, pas un binding.

## Ce qui est déjà fait et réutilisable

- `struct iwx_scd_queue_cfg_cmd_add` + `IWX_SCD_QUEUE_CONFIG_CMD` (0x17)
  dans `if_iwxreg.h` (d'après Linux commit `227f25972c5a`). Vérifié : le
  firmware la reçoit et la traite.
- `iwx_tvqm_enable_txq`/`iwx_tvqm_alloc_txq` acceptent `new_sta_mask`
  (0 = ancienne commande) — le chemin DMA d'anneau est partagé.
- Rate-scaling : `iwx_rs_init` (TLC) déjà câblable (binding 8, `tlc=1`
  accepté par le firmware).

## Roadmap

1. **Récupérer les structures + le code appelant `iwlmld`** (pas juste
   les structs) : `fw/api/mac-cfg.h` (MAC_CONFIG, LINK_CONFIG),
   `fw/api/sta.h` récent (STA_CONFIG), et le driver `iwlmld` qui les
   remplit. La reconstruction à partir des seules structs a suffi pour
   que le firmware *reçoive* la commande de file ; pour la station il
   faut le code appelant (flags, link_id, sta_mask alloué, etc.).
2. **Option plus fiable** : capturer les **octets exacts** des commandes
   d'owly sous Linux (kprobe/bpftrace sur l'enqueue hcmd, ou l'event
   `iwlwifi_dev_hcmd` avec données) pour répliquer octet pour octet au
   lieu de reconstruire. La trace `trace-cmd -e iwlwifi:*` actuelle ne
   donne que les id, pas les payloads.
3. Implémenter dans l'ordre : MAC_CONFIG → LINK_CONFIG → STA_CONFIG →
   SCD_QUEUE_CONFIG, puis TLC, dans un chemin d'injection AWDL dédié.
4. Garder le Bluetooth (BLE Continuity) comme verrou séparé pour le
   sens PC→iPhone (le /Discover) — hors radio.

## Matrice des bindings (debug.itlawdl_txsta)

| b | station | file | résultat |
|---|---|---|---|
| 0 | monitor GP, @0 | 2 statique | assert, done=0 |
| 1 | sta0 LINK, @nous | 2 statique | assert 0x103d |
| 2 | aux, @nous | 2 statique | assert |
| 3 | sta0 mcast, @ff | 2 statique | assert |
| 4 | snif_sta GP @0 tid8 | 2 statique | assert (= forme mvm) |
| 5 | sta0 gp bcast | 2 statique | assert |
| 6 | 5 + session protection | 2 statique | assert (prot=0, bug) |
| 7 | sta0 LINK | TVQM (→2) | assert 0x71, done=0 |
| 8 | 7 + TLC | TVQM (→2) | assert, tlc=1 OK |
| 9 | sta0 LINK | **SCD_QUEUE_CONFIG** nouvelle API | assert `d1=0x17 d2=0x5` — station pas nouvelle API |

## Références

- owly (t4t5/owly, MIT) — fait l'injection sur Linux avec iwlwifi.
- Linux commit 227f25972c5a (Johannes Berg) — `iwl_scd_queue_cfg_cmd`.
- Trace terrain : `awdl-trace.txt` (owly test-card, AX211, 2026-10-24).

---

## Recette exacte d'owly (octets capturés 2026-10-24, bpftrace sur iwlwifi_dev_hcmd)

Chaîne d'injection owly (nouvelle API iwlmld). Substituer notre MAC à
`40:d1:33:7a:3e:d3`, nos adresses DMA à celles de la file. Le BSSID AWDL
`00:25:00:ff:94:73` est déjà celui qu'on veut.

### MAC_CONFIG_CMD (0x03.0x08), len 52 — contexte d'injection
```
id_and_color=0  action=ADD(1)/MODIFY(2)  mac_type=4
local_mld_addr=<NOTRE MAC>  filter_flags=0x2c
he_support=0 he_ap=0 eht=0  nic_not_ack_enabled=1  union=0
```
Champs (le32 sauf addr) : id_color@0, action@4, mac_type@8,
local_mld_addr[6]@12 +rsv[2]@18, filter_flags@20, he_support(16)@24,
he_ap_support(16)@26, eht_support@28, nic_not_ack@32, union[16]@36.

### LINK_CONFIG_CMD (0x03.0x09), len 208 — lien d'injection
```
action=MODIFY(2)  link_id=0  mac_id=0  phy_id=0
local_link_addr=<NOTRE MAC>  modify_mask=3  active=1
cck_rates=0x0f  ofdm_rates=0x15  protection_flags=6  qos_flags=3
ac[0]=cwmin0x0f cwmax0x3ff aifsn7 fifo2 txop0
ac[1]=cwmin0x0f cwmax0x3ff aifsn3 fifo4 txop0
ac[2]=cwmin0x07 cwmax0x0f aifsn2 fifo8 txop0x0bc0
ac[3]=cwmin0x03 cwmax0x07 aifsn2 fifo16 txop0x05e0
ac[4]=0
bi=10000(0x2710)@136  bssid 00:25:00:ff:94:73 @168
```
Offsets : action@0 link_id@4 mac_id@8 phy_id@12 addr[6]@16+rsv@22,
modify_mask@24 active@28 union@32 cck_rates@36 ofdm_rates@40
cck_short_pre@44 short_slot@48 protection_flags@52 qos_flags@56
ac[5]@60 (8o chacun) … bi@136 dtim@140 … bssid[6]@168.

### STA_CONFIG_CMD (0x03.0x0a), len 96 — station d'injection
```
sta_id=2  link_id=0  peer_mld=00:25:00:ff:94:73  peer_link=00:25:00:ff:94:73
station_type=1 (BCAST_MGMT)  assoc_id=0  beamform=0  mfp=1  reste=0
```
Offsets : sta_id@0 link_id@4 peer_mld[6]@8+rsv@14 peer_link[6]@16+rsv@22
station_type@24 assoc_id@28 beamform@32 mfp@36 mimo@40 …

### SCD_QUEUE_CONFIG_CMD (0x05.0x17), len 36 — file d'injection
```
operation=ADD(0)  sta_mask=BIT(2)=4  tid=15  flags=0  cb_size=4
bc_dram_addr=<notre bc_tbl>  tfdq_dram_addr=<notre desc_dma>
```
(La struct iwx_scd_queue_cfg_cmd_add du binding 9 est déjà bonne.)

### Ordre owly
MAC_CONFIG(ADD) → LINK_CONFIG(ADD) → LINK_CONFIG(MODIFY active=1) →
STA_CONFIG(bcast_mgmt) → SCD_QUEUE_CONFIG(ADD). Puis l'injection sort sur
la file rendue (owly : queue 0x1c=28). TLC (0x05.0x0f) en plus pour les
stations peer, pas nécessaire pour l'injection bcast_mgmt a priori.

### Inconnue à valider
phy_id=0 : suppose que le contexte PHY existant d'itlwm (phy_ctxt[0],
IWX_PHY_CONTEXT_CMD) est compatible. Si assert sur phy, c'est le premier
point à corriger (passer le PHY à la nouvelle API aussi).

---

## v2.8.5 / v2.8.6 — commandes acceptées, mais contexte non activé

Le chemin nouvelle API complet (binding 10, iwx_awdl_mld_inject_setup)
envoie MAC_CONFIG → LINK_CONFIG(ADD+MODIFY active) → STA_CONFIG(bcast_mgmt)
→ SCD_QUEUE. Résultats terrain :

- **Aucun plantage au setup** : les 4 commandes sont acceptées (grand
  pas — fini les asserts qui nomment nos commandes au setup).
- **rx=0** : le contexte ne reçoit rien, même après LINK MODIFY active=1.
- **Assert à l'émission** : fw_err monte dès la 1re trame, `umac d1=0x8
  d2=0x3` = MAC_CONFIG (0x3.0x8). Le firmware trouve le contexte
  incomplet quand la TX l'utilise. La file rendue par TVQM = qid 2 (owly
  obtenait 28), signe d'un contexte pas pleinement actif.

### Cause : il manque tout le rituel de bring-up MLD

La capture montre qu'owly fait ~100 commandes interdépendantes AVANT que
le contexte d'injection soit actif, pas seulement les 4 de la chaîne :
commandes système (2.0f), base PHY (1.a9, 1.d2), data-path (5.16, 5.02,
5.0f TLC, 5.08 RLC), per-chain TX power (4.04, 4.05), un BEACON_TEMPLATE
(1.91), et plusieurs MAC/LINK MODIFY. Reproduire seulement MAC/LINK/STA/
QUEUE fait accepter les commandes mais n'active pas le contexte.

Séquence owly autour de l'injection (fin des 100 premières) :
`… 1.08 5.08 1.91 3.09 3.0a 5.17 3.0a 5.17 3.09 1.91`
(RLC, beacon, LINK, STA mcast, queue, STA bcast_mgmt, queue, LINK, beacon)

### Conclusion

Le correctif complet = porter le **bring-up MLD** d'iwlmld (pas juste 4
commandes) : la config système/PHY-DB/data-path nouvelle API, le beacon
template, et l'ordre exact. C'est un vrai portage de sous-système, à
faire posément (idéalement avec la source iwlmld + les octets déjà
capturés dans hcmd-bytes). Tous les octets des commandes individuelles
sont connus (capture 2026-10-24) ; ce qui reste, c'est l'orchestration.
