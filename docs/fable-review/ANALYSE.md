# Réexamen du blocage AWDL sur AX211

## Conclusion vérifiable

L'affirmation « la fenêtre 82–98 est reproduite à l'octet près, donc il faut porter tout le boot iwlmld » n'est pas démontrée. Une divergence existe déjà dans cette fenêtre : les deux commandes RLC utilisent la version d'en-tête **2** sous Linux et **0** dans `MLD_SEND`. Le patch corrige cette divergence, sans prétendre qu'elle explique à elle seule l'assert TX.

Les cinq pièces ont été lues. Le fichier C++ joint est un extrait incomplet ; la suite réelle de la fonction TX et le transport des commandes ont donc été vérifiés dans le dépôt complet. Le fichier distant de référence est le blob `961ccb8e6a52f9cf0003806b571eb5624b73ea11` de `itlwm/hal_iwx/ItlIwx.cpp`, branche `awdl-v2`. Certaines parties du dépôt ont évolué par rapport à l'extrait : les patchs ciblent ce blob, pas les numéros de ligne de l'ancien rapport.

## 1. Format réel et limites de la capture

Après `HCMD `, les octets correspondent directement à l'en-tête large Intel. Il n'y a pas de préfixe indépendant de longueur sur quatre octets dans ces enregistrements.

| Offset | Taille | Champ |
|---|---:|---|
| 0 | 1 | opcode |
| 1 | 1 | groupe |
| 2 | 2 | séquence, little-endian |
| 4 | 2 | longueur du corps, little-endian |
| 6 | 1 | réservé |
| 7 | 1 | version d'en-tête |
| 8 | longueur annoncée | corps |

Cette disposition est celle de `iwl_cmd_header_wide` [S1]. Lire les octets 4–7 comme un entier de longueur donne par exemple `0x02000020` pour RLC, au lieu de **32 octets + version 2**.

Les 283 enregistrements décodés font chacun 400 octets. Ils contiennent au maximum 392 octets de corps. Les octets après la longueur utile sont exclus des comparaisons ; ils peuvent contenir des résidus et ne constituent pas des paramètres supplémentaires.

| Indice, base 0 | Commande | Longueur annoncée | Corps disponible | Octets manquants |
|---:|---|---:|---:|---:|
| 70 | `05.0d` RFH_QUEUE_CONFIG | 452 | 392 | 60 |
| 157 | `05.0d` RFH_QUEUE_CONFIG | 452 | 392 | 60 |
| 185 | `01.0d` SCAN_REQ_UMAC | 1940 | 392 | 1548 |

Il est donc faux que cette capture contienne tous les octets de toutes les commandes. Une capture plus longue est nécessaire pour ces trois corps. Les commandes de configuration RX contiennent en outre des adresses propres à l'exécution : les recopier depuis Linux ne remplace pas l'allocation et la configuration DMA côté macOS.

## 2. Différence RLC confirmée

Les douze tableaux `mld_*` comparés à leur commande de référence ont un **corps identique** : `mac_add`, `link_add`, `rlc1`, les quatre `link_mod`, `cmd_9f`, `rlc2`, `beacon`, `sta_mcast`, `sta_bcast`.

| Envoi | Capture | Code avant correction | Code candidat |
|---|---|---|---|
| RLC indice 86 | `05.08`, 32 octets, version 2 | version 0 | version 2 |
| RLC indice 91 | `05.08`, 32 octets, version 2 | version 0 | version 2 |

Le transport `iwx_send_cmd` écrit bien `iwx_cmd_version(code)` dans `hdr_wide.version` : il ne remplace pas automatiquement le zéro par la version annoncée du firmware. Le code Linux `mvm/phy-ctxt.c` envoie explicitement RLC avec `iwl_cmd_id(..., 2)` [S2]. Les cinq en-têtes non nuls de la capture sont tous des RLC : indices 86, 91, 192, 194 et 220.

Le contrôle ajouté exige `rlc_ver == 2` pour ce rejeu précis. Il n'impose pas arbitrairement la version 2 à un firmware avec une autre ABI. La version annoncée dans les TLV et l'octet transmis dans l'en-tête sont deux informations distinctes ; on ne remplace pas tous les en-têtes par leurs versions TLV.

## 3. Les indices 0–81 ne sont pas un bloc uniforme d'initialisation

La capture commence avec une séquence à 121 et des opérations sur des contextes déjà existants. Elle contient ensuite des suppressions de ressources, puis un redémarrage de la séquence à l'indice 62. Voici des éléments directement observables :

- 42–44 : `SCD_QUEUE_CONFIG`, opération **REMOVE (1)**.
- 47 : suppression de station (`03.0c`).
- 50, 54, 57 : `LINK_CONFIG`, action **REMOVE (3)**.
- 55, 58 : `MAC_CONFIG`, action **REMOVE (3)**.
- 62 : `02.03 INIT_EXTENDED_CFG_CMD`, séquence remise à 0.
- 62–81 : vingt commandes avant le nouveau `MAC_CONFIG ADD` de l'indice 82.
- 149 : nouvelle occurrence de `INIT_EXTENDED_CFG_CMD`, avec séquence 0.

L'interprétation la mieux étayée est une trace comportant plusieurs phases de vie, dont un arrêt et une réinitialisation. L'absence de timestamps et de notifications ALIVE dans ce fichier ne permet pas de reconstituer toute la chronologie matérielle. Il faut isoler les epochs avant d'attribuer une dépendance à une commande précédente.

Plusieurs noms donnés dans le rapport doivent être corrigés [S3, S4] :

| Identifiant | Nom dans les sources Linux | Conséquence pour l'analyse |
|---|---|---|
| `02.0f` | SYSTEM_STATISTICS_CMD | Ne prouve pas une activation MLD |
| `01.a9` | MAC_PM_POWER_TABLE | Ce n'est pas PHY-DB |
| `01.d2` | BEACON_FILTERING_CMD | Ce n'est pas PHY-DB |
| `05.16` | RX_BAID_ALLOCATION_CONFIG_CMD | Gestion des sessions de réordonnancement RX |
| `05.02` | TRIGGER_RX_QUEUES_NOTIF_CMD | Synchronisation/notifications RX |
| `05.18` | SEC_KEY_CMD | Gestion des clés, pas une initialisation générique du scheduler |

Le bloc précédent peut donc inclure du nettoyage d'une association passée, pas des prérequis à recopier avant une cellule AWDL.

## 4. API MLD, module iwlmld et firmware réellement chargé

Les sources Linux contiennent `iwl_mvm_has_mld_api()` **dans le sous-pilote mvm**. Cette fonction examine `IWL_UCODE_TLV_CAPA_MLD_API_SUPPORT`, bit **110** [S5, S6]. La présence des commandes MAC_CONFIG/LINK_CONFIG ne suffit donc pas à affirmer que le module `iwlmld` est chargé.

Le pilote itlwm récupéré stocke déjà 128 bits de capacités dans `sc_enabled_capa` et possède `iwx_lookup_cmd_ver()`. Il peut inspecter cette capacité et les versions sans inventer une commande qui « active le mode MLD ». Le patch ajoute ce diagnostic avant le rejeu et refuse une configuration non annoncée compatible. Ce contrôle est nécessaire mais ne démontre pas que les autres modèles de commandes correspondent à toutes les versions du firmware.

Le rapport donne un firmware macOS `so-a0-gf4-a0-68`, mais la capture HCMD seule ne fournit pas la version de l'image Linux. **Même carte ne signifie pas même firmware ou même ABI.** Il faut récupérer les lignes Linux `loaded firmware ... op_mode ...`, la version du noyau et les versions de commandes, puis comparer au journal macOS ajouté par le patch. Cette comparaison manque actuellement.

## 5. Files TX et autres défauts concrets

Le rapport ne permet pas de conclure que la file 2 est inutilisable. La définition historique Linux réserve même le numéro 2 à l'injection monitor ; cette constante ne prouve toutefois pas à elle seule les règles de TVQM [S7]. La bonne règle est d'utiliser le numéro réellement alloué, d'en contrôler le statut et de vérifier sa compatibilité avec les ressources du pilote. Le fichier HCMD fourni ne contient pas les réponses SCD permettant de vérifier indépendamment le numéro 28 annoncé par les rapports.

L'ancien et le nouveau SCD utilisent bien la même structure de réponse `iwl_tx_queue_cfg_rsp` [S4, S7] : aucune preuve ici d'un décalage de champs expliquant le 2. Le patch journalise `qid`, `flags`, `wrptr` et la taille effective. La gestion complète des statuts d'erreur, des collisions et du retrait d'une allocation firmware lors d'un échec reste à auditer avant usage durable ; le patch de journalisation ne prétend pas la résoudre.

Le code d'injection recyclait un emplacement occupé en supposant que le firmware avait lu son buffer après environ 14 secondes. L'écoulement du temps ne garantit pas une fin d'accès DMA. Le second patch conserve le buffer et sa référence de nœud et renvoie `ENOBUFS`. Cela corrige un défaut indépendant sans prétendre expliquer le premier crash, qui survient avant un tour complet de l'anneau.

Deux lectures nécessitaient aussi des contrôles supplémentaires : l'index de file avant `sc->txq[qid]`, et l'en-tête complet avant sa copie ou l'accès à `i_addr3`. Le patch ajoute ces contrôles ainsi qu'une borne sur l'indice MCS. Il ne constitue pas un audit exhaustif des chemins RX/TX, des débits autorisés, de la concurrence ou de la récupération après crash.

## 6. Réponses aux cinq questions du rapport

1. **Initialisation minimale :** impossible à établir complètement à partir de ces seules commandes. La première expérience justifiée est la vérification des ABI et la correction RLC, puis l'examen des vingt commandes 62–81 et du code qui les émet. Rejouer 0–81 inclurait des suppressions et des données tronquées.
2. **Sens des asserts :** aucune correspondance symbolique vérifiée n'a été obtenue pour ces adresses d'assert dans l'image exacte. Interpréter automatiquement `d1=8, d2=3` comme la preuve d'un MAC_CONFIG incomplet dépasse les données disponibles. `queued=1` et `done=0` ne prouvent pas non plus que le firmware a effectivement lu le TFD.
3. **Forcer une file haute :** pas justifié par les preuves. Il faut d'abord vérifier la réponse et les réservations du pilote ; le numéro 28 n'est pas un paramètre de fonctionnement à recopier.
4. **Mélanger les API :** les familles de commandes ont des versions et capacités propres ; le groupe 1 reste également utilisé dans la trace à commandes MLD. Cela ne démontre pas qu'un contexte legacy et une station MLD sont compatibles entre eux. L'interdiction globale d'un mélange et son contraire ne sont pas établis pour l'image chargée.
5. **Négociation/activation :** examiner le bit 110 et la table de versions déjà lus par itlwm. Aucun « bit magique » d'activation à envoyer n'est identifié. Adapter les ABI supportées est différent de modifier arbitrairement les capacités annoncées.

## 7. Validation effectuée et limites

- Décodage des **283 commandes**, avec détection de **3 corps tronqués**.
- Comparaison exacte réussie des **12 modèles** présents dans l'extrait.
- **6 tests Python** réussis : version séparée de la longueur, troncature, caractères imprimables, en-tête court, échappement invalide et lignes ignorées.
- Application séquentielle des **2 patchs** vérifiée avec `git apply --check`, puis comparaison exacte du résultat au fichier corrigé.
- Compilation d'un banc C++ avec AddressSanitizer et UndefinedBehaviorSanitizer : **9 cas** du préambule TX réel extrait du fichier corrigé réussissent, dont la préservation d'un buffer et de son nœud encore en vol.
- **Non effectué :** compilation du projet kext complet, chargement du pilote, mesure radio AX211, validation des complétions réelles, découverte/transfert AirDrop. Le banc C++ utilise des objets simulés et ne remplace aucun de ces essais.

## Sources primaires consultées

- [S1 — En-têtes de commandes Intel](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/fw/api/cmdhdr.h), blob lu `073f003bdc5d2a55850a3d877b310520c7f676ce`.
- [S2 — Appelant RLC dans mvm](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/mvm/phy-ctxt.c), blob lu `4f4111055ddd249f5308f35c65c5c63df7c0c89b`.
- [S3 — Identifiants LONG et SYSTEM](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/fw/api/commands.h).
- [S4 — Identifiants et structures DATA_PATH](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/fw/api/datapath.h), blob lu `eddf13325df95b2f334ab56d451ba562bcc820e6`.
- [S5 — Détection de l'API MLD dans mvm](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/mvm/mvm.h).
- [S6 — Capacités firmware](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/fw/file.h), blob lu `3a986b394dd822e4218d5101fbdbacf8291526e3`.
- [S7 — Files et réponse d'allocation](https://github.com/torvalds/linux/blob/master/drivers/net/wireless/intel/iwlwifi/fw/api/txq.h), blob lu `e6c0f928a6bbf338ca240214635313c05c4e8751`.
- [Code complet du fork examiné](https://github.com/MrKenny13/itlwm/blob/awdl-v2/itlwm/hal_iwx/ItlIwx.cpp) et [définitions du fork](https://github.com/MrKenny13/itlwm/blob/awdl-v2/itlwm/hal_iwx/if_iwxreg.h).

Les sources Linux servent à vérifier les définitions et le comportement de leur appelant ; elles ne prouvent pas la compatibilité de l'image macOS 68 avec une ABI plus récente. Les assertions de réussite radio et le numéro 28 restent attribués aux rapports fournis, non reproduits pendant cette analyse.
