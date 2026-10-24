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
