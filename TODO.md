# TODO — audit du 2026-10-24

Audit complet du dépôt (lecture + exécution des tests en local, y compris les suites
Postgres contre une base jetable). Charge supposée : une organisation, 2 réplicas,
quelques centaines à un millier de flux SSE, rafales de publications.

Chaque entrée : où, quoi, le cas concret, le plus petit correctif. Cocher quand c'est
corrigé **et** testé. Les constats marqués *plausible* n'ont pas été tracés jusqu'au bout.

## Must fix

- [x] **1. Clé API « canal » : propriétaire jamais vérifié** — `crates/notify-app/src/keys.rs:176-204`
  - `CreateApiKey` vérifie `manage` sur `request.channels`, jamais que le canal propriétaire y figure ni qu'il existe.
  - Cas : Alice gère `ci-infra`, crée une clé propriétaire `payroll` (ou `inexistant`) avec `publish` sur `ci-infra`. Invisible dans le listing de `ci-infra` (filtré par propriétaire), révocable par personne (pas de repli admin dans `administers_key`), survit à son départ du canal.
  - Fix : refuser si `request.channels` ne contient pas l'`id` propriétaire ; `caller.platform_admin ||` dans la branche canal de `administers_key`. Un test « a channel key must name its owner among its channels ».

- [x] **2. Tauri envoie le token OIDC à l'URL du profil CLI, `http://localhost:8080` par défaut** — `web/src-tauri/src/lib.rs:208-231`, `crates/notify-client-core/src/config.rs:34`
  - `credential_session` recharge `Profile::load(DEFAULT_PROFILE)` ; sans fichier, `Profile::default()` pointe sur `http://localhost:8080`. Le tray interroge cette URL toutes les 20 s avec `Authorization: Bearer <token>`.
  - Fix : passer `apiBaseUrl` avec le token (`credential_session(token, name, url)`), l'imposer dans `profile.url`, refuser `http://` hors loopback en dev.

- [x] **3. Publier avec une référence d'image non vérifiée : suppression d'avatar et lecture inter-canaux** — `crates/notify-app/src/publish.rs:113-131`, `crates/notify-adapters/persistence-pg/src/notifications.rs:700-733`, `crates/notify-app/src/maintenance.rs:286-310`
  - `image: {kind: "attachment", ref}` n'est pas vérifié contre le registre des présignatures du canal. Avatars et pièces jointes partagent le même magasin et le même type d'id. `orphaned_attachments` ne regarde pas `users.avatar`.
  - Cas : publier avec l'avatar d'un collègue (id visible via `GET /users`), supprimer, attendre 7 jours : l'objet est effacé, `/avatars/{id}` répond 422. Variante : republier dans B un id de pièce jointe appris dans A.
  - Fix : refuser une `Image::Attachment` absente du registre pour ce canal. Palliatif : `AND NOT EXISTS (SELECT 1 FROM users WHERE avatar = …)` dans le balayage + idem dans `InMemory::orphaned_attachments`.

- [x] **4. Chaque poke réveille chaque flux du réplica, ~10 requêtes dont 2 écritures par réveil** — `crates/notify-adapters/http-actix/src/routes/stream.rs:361-366`, `crates/notify-app/src/sync.rs:76-126`
  - `pokes.next()` met `more = true` sans filtre canal/curseur ni regroupement ; `SyncBacklog` enchaîne `remember_groups`, `devices.get`, `readable_channels`, `visible_channels`, `settle`, `sync_page`, `compacted`, `expired_while_away`, `dnd`, `advance_cursor`.
  - Cas : 1 000 flux × 100 publications/s = 100 000 syncs/s sur un pool de 16 ; acquisitions expirées → flux fermés → reconnexions → tempête auto-entretenue.
  - Fix : ignorer `Poke::Channel` hors ensemble lisible ou avec curseur ≤ courant, ignorer `Poke::User` d'autrui, vider le récepteur (`try_recv`) avant de paginer. Sortir `remember_groups` vers `Connection::open`.

- [x] **5. La purge fait un `head()` + NOTIFY par notification expirée** — `crates/notify-app/src/maintenance.rs:183-193`
  - `CloseStale` (`closure.rs:104-121`) fait déjà un `head()` et un poke par canal par lot.
  - Fix : copier ces 8 lignes (tri + dédoublonnage des canaux).

- [x] **6. Une clé personnelle peut énumérer l'annuaire** — `crates/notify-app/src/accounts.rs:63-66`
  - `SearchUsers` refuse les non-`User` ; une clé personnelle est `Principal::User`.
  - Cas : clé `publish` fuitée → `GET /users?q=aa`, `?q=ab`… → noms et emails de tout le monde.
  - Fix : `caller.ensure_person()?` à la place du `matches!`. Une assertion dans `authorisation_boundaries.rs`.

- [x] **7. Cache SQLite client créé avec l'umask par défaut** — `crates/notify-client-core/src/store.rs:102-130`
  - `create_dir_all` + `Connection::open` sans mode ; config et credential voisins sont en 0600 (`config.rs:220-270`).
  - Fix : `set_permissions(parent, 0o700)` après `create_dir_all`, sous `cfg(unix)`.

- [x] **8. CI inopérante depuis le 2026-10-09 (facturation GitHub)** — *Résolu le 2026-10-25 : dépôt passé public, les workflows tournent (check, e2e, contract, audit, oidc-compat ×3, codeql, semgrep, secret-scan, publish-main, release).*
  - Tous les jobs échouent en 3 s. La suite `critical` et les tests Postgres ne tournent nulle part. En local, `cargo test` (sans nextest) fait échouer 3 tests `watermark` par contention de base ; seul `.config/nextest.toml` les isole.
  - Fix : régler la facturation ou lancer la migration Forgejo prévue. En attendant, avant chaque tag : `WNK_TEST_DATABASE_URL=… WNK_TEST_REQUIRE_DB=1 cargo nextest run --workspace -P critical`.

## Should fix

- [x] **9. `token_secret` sans longueur minimale** — `crates/notify-server/src/config.rs:941-943`, `doctor.rs:112-128`
  - Seul « non vide » est exigé ; `fingerprint_key` impose `MIN_BYTES`. `"changeme"` passe → tokens d'upload (25 MiB, hors registre, jamais purgés) et de download forgeables.
  - Fix : même règle que `FingerprintKey::MIN_BYTES` dans `Config::validate` ; supprimer l'heuristique placeholder de `doctor`.

- [x] **10. Identifiants SMTP en clair sur `smtp://`** — `crates/notify-adapters/sinks-email/src/lib.rs:56-64`, `config.rs:608-615` *(plausible)*
  - lettre mappe `smtp://host` sans `?tls=` sur `Tls::None`, pas de STARTTLS.
  - Fix : si `username` est défini, exiger `smtps://` ou `?tls=required` dans la validation.

- [x] **11. Clé révoquée : flux ouvert jusqu'à 5 min** — `crates/notify-adapters/http-actix/src/routes/stream.rs:28`
  - `REAUTHENTICATE = 300 s` ; le retrait de membre est immédiat, `revoked_at` n'est relu qu'au tick. L'ADR-0023 dit « révocation immédiate ».
  - Fix : 60 s + corriger l'ADR, ou `streams.disconnect` par empreinte depuis `RevokeApiKey` / `DeactivateServiceAccount`.

- [x] **12. Un lot d'expiration de 1 000 lignes retient le watermark** — `crates/notify-adapters/persistence-pg/src/notifications.rs:514-558`, `events.rs:198-231`
  - 3 allers-retours par ligne dans une transaction ; pendant ce temps `settle()` sonde toutes les 50 ms (5 s max) depuis chaque flux, puis `Resync` réveille tout le monde.
  - Fix : plafonner `claim_expired` indépendamment de `[retention] batch_size` (`limit.min(100)`, comme `CloseStale` `BATCH = 200`) ; plancher de sondage de `settle` à 100 ms.

- [x] **13. Leader partitionné : aucun timeout client** — `crates/notify-adapters/persistence-pg/src/leader.rs:74-110`, `lib.rs:117-142` *(plausible)*
  - `is_held()` = `SELECT 1` sans timeout ; verrou de session. Coupure TCP silencieuse → zéro leader (purge, close-stale, digests, sink-drain) jusqu'au timeout noyau, `/readyz` vert. `release()` remet la connexion dans le pool si `pg_advisory_unlock` échoue.
  - Fix : `tokio::time::timeout(5 s)` autour de `is_held` et `try_acquire` (timeout = perdu, `take()` la connexion) ; `close_on_drop()` dans la branche d'erreur de `release()` ; `tcp_user_timeout` dans les options de connexion.

- [x] **14. Fan-out : silences et DND lus par appareil, pas par utilisateur** — `crates/notify-app/src/fanout.rs:245-265, 306-361`
  - 300 abonnés × 2 appareils = 1 200 lectures par notification, répétées à chaque tentative.
  - Fix : résoudre « quiet ou non » une fois par propriétaire distinct avant le `for_each_concurrent`, map consultée dans `push_one`.

- [x] **15. `/api/docs` charge Scalar depuis `cdn.jsdelivr.net` sans version ni SRI** — `crates/notify-server/src/main.rs:286-291`, `config.rs:201,212` *(plausible : template utoipa-scalar 0.3 lu, 0.4 non vérifié)*
  - Page activée par défaut, sans auth, JS tiers hors de la chaîne épinglée revendiquée par l'ADR-0023 §8 (adversaire chaîne d'approvisionnement).
  - Fix : `docs = false` par défaut, ou `Scalar::custom_html` avec version épinglée + `integrity=`.

- [x] **16. Tests manquants**
  - [x] HTTP : `/admin/*` → 403 pour une session non admin et pour une clé (`http-actix/tests/api.rs`).
  - [x] HTTP : une clé révoquée est refusée à la requête suivante ; fermeture du flux à la révocation.
  - [x] Points 1 et 6 (owner canal, `SearchUsers` refuse une clé).
  - [x] `verifier.rs` : token `alg: HS256`/`none` avec le `kid` d'une clé RSA/EC publiée ; refus de `typ` (`verifier.rs:264-270`) ; `kid` absent du set après refresh ; `iss` avec slash final sur la politique fédérée (`notify-domain/src/federation.rs:200`).
  - [x] Postgres : exclusion par notification (`FOR SHARE` vs `claim_expired`/`close_stale` `SKIP LOCKED`) ; `settle()` abandonne à 5 s ; `watch_watermark` arme et re-poke (`notify-server/src/bus.rs:208-258`) ; `insert_with` retire une ligne stale tenue par `claim_expired` ; `is_held` → false à la perte de connexion.

## Nice to have

- [x] **17. Oracle d'existence : id inconnu → 409, existant mais interdit → 403** — `crates/notify-app/src/read.rs:166-174, 322-331`. Le commentaire L159-160 dit le contraire. Mapper `None` sur `ScopeMissing { scope: "subscribe" }`, ou corriger le commentaire.
- [x] **18. `expired_while_away` et `compacted` comptent au-dessus du watermark** — `crates/notify-adapters/persistence-pg/src/events.rs:141-155, 296-325`. Ajouter `AND seq <= events_watermark()`.
- [x] **19a. `Idempotency-Key` stocké sans borne** — `routes/notifications.rs:139-143`, PK `key TEXT`. `.filter(|k| k.len() <= 255)` → 422.
- [x] **19b. Rejeu fédéré indexé sur `jti` seul** — `persistence-pg/src/federation.rs:14-18`, `notify-app/src/federate.rs:128-129`. Mémoriser `format!("{issuer}|{jti}")`.
- [x] **20a. TUI : markdown brut passé au démon de notifications** — `notify-cli/src/tui/desktop.rs:17-25`, `tui/mod.rs:169` *(plausible)*. Échapper `& < >` avant `.body()`.
- [x] **20b. Listener OAuth loopback sans limite de lecture** — `web/src-tauri/src/oauth.rs:196-199`. `BufReader::new(peer.take(8 * 1024))`.

## Petits constats laissés hors du rapport

- [x] Le relais push voit la longueur exacte du texte en clair (sealed box sans padding) — `sealed-box/src/lib.rs:27-84`, ADR-0023 §5 dit « timing et priorité APNs » seulement. Padder à 256 octets via une clé JSON ignorée, ou corriger l'ADR.
- [x] Lignes `dead_at` du fan-out jamais purgées ; `fanout_backlog` est un `count(*)` complet — `persistence-pg/src/outbox.rs:283-308`. Un DELETE borné dans la passe de purge.
- [x] Deux révisions drainées sur deux réplicas : l'ancienne peut arriver en dernier — `fanout.rs:568-589`, `outbox.rs:117-135`. `NOT EXISTS` sur une ligne louée de la même notification dans le claim.
- [x] `GET /apikeys` en O(comptes × clés × authorize) ; `GET /serviceaccounts` sans contrôle de principal — `keys.rs:284-302`, `accounts.rs:36-38`. Filtrer par `created_by` d'abord ; `ensure_session()`.
- [x] Pool de 16 partagé entre requêtes, 4 gardes leader, sondeurs `settle` et flux — `persistence-pg/src/lib.rs:56-67`, `wiring.rs:437-490`. Une élection partagée pour les 4 jobs, ou 32 après le point 4. — *gardé à 16 : la pression venait des flux (point 4) et de `settle` (point 12) ; 32 × 3 réplicas en rolling update frôle le `max_connections` 100 de Postgres.*
- [x] Commentaire du chart devenu faux : « empty means open » pour `tokenSecretKey` — `charts/weebo-notify/values.yaml`. Le serveur refuse un secret vide.
- [x] Unpaginated : `ListServiceAccounts`, inventaire plateforme (`platform.rs:62-80`), toutes les interactions (`interact.rs:120-127`). Coût seulement, appelants authentifiés. — *comptes de service et inventaire paginés ; interactions laissées non paginées, choix de conception documenté sur le port (§8 : une approbation sondée ne doit jamais être manquée).*

## Non vérifié

Lu partiellement ou pas du tout. À couvrir lors d'une prochaine passe.

- `crates/notify-server/src/reconcile.rs` (grep seulement) ; `main.rs` hors L230-350 et L530-700 ; `config.rs` L1485-1828 (tests).
- `crates/notify-adapters/push-webpush/src/encrypt.rs`.
- `crates/notify-adapters/oidc/src/lib.rs`, `test_support.rs` ; `oidc/tests/oidc_compat.rs` (noms des tests seulement).
- `crates/notify-adapters/http-actix/tests/api.rs` (noms seulement, 2 531 lignes) ; `routes/preferences.rs`, `routes/push.rs`, `routes/attachments.rs` (signatures seulement) ; `slots.rs` et le wrapper `supervise` (borne-t-il une passe avec un timeout ? pèse sur le point 13).
- `crates/notify-app/src/` : `platform.rs` après L260, `maintenance.rs` après L330, `sync.rs` après L330, `profiles.rs`, `preferences.rs`, `digest.rs`, `attribution.rs` (hors branche `ChannelKey`), `sinks.rs` L560-988, et les moitiés tests de `publish.rs` (L420+), `interact.rs` (L560+), `read.rs` (L420+), `channels.rs`, `devices.rs`, `accounts.rs`, `attachments.rs`.
- `crates/notify-adapters/persistence-pg/src/{attachments,declarations,federation,identities}.rs` ; migrations autres que initial, watermark, outbox, leases, index, sink_queue, audit_purge ; `tests/conformance.rs` hors L333-380, L3013-3160, L3600-3640.
- `crates/notify-ports/src/test_utils/store.rs` hors L380-500 et L1780-1890.
- `crates/notify-adapters/attachments-s3/tests/conformance.rs` ; non vérifié que rust-s3 inclut les en-têtes personnalisés dans `SignedHeaders` des URL présignées.
- `crates/notify-client-core/src/{dnd,election,signal,sync,tray,error,lib}.rs` ; `store.rs` L420-908.
- `crates/notify-cli/src/{manage,publish}.rs` ; `tui/{app (hors L730-800),completion,compose,history,menu,render}.rs`.
- `web/src/lib/push.ts` (grep seulement), `auth/presence.ts`, `stores/*`, `lib/markdown.spec.ts`.
- Source de `utoipa-scalar` 0.4.0 (seule la 0.3.0 était dans le registre local).
- Aucun test de charge exécuté : les chiffres des points 4, 5, 12 et 14 sont calculés depuis le code, pas mesurés.
- Aucun build d'image ni scan `trivy image` exécuté (pas de registre local) ; `task audit:container` et `audit:helm` non lancés.
- Clippy / `task lint` non relancés (hooks pre-commit supposés verts sur `main`).

---

# Revue du 2026-10-24 (après f2032ed)

Constats vérifiés dans le code, puis corrigés avec un test qui échoue sans le correctif
(sauf mention contraire).

- [x] **1. Une clé `publish` lisait tous les canaux de son propriétaire via `/sync` et `/stream`** — `notify-app/src/sync.rs` : une liste résolue vide ne veut plus dire « tout ».
- [x] **2. `GET /channels` avec une clé listait les canaux du propriétaire, membres compris** — `channels.rs` : filtré par `readable_channels` hors session.
- [x] **3. Le self-join admin rétrogradait l'admin et écrasait les modifications concurrentes** — `platform.rs` : `put_member`, appartenance existante conservée.
- [x] **4. (régression f2032ed) Image refusée après expiration de sa notification** — `attachment_in_channel` (vivante ou supprimée avant la purge).
- [x] **5. (régression f2032ed) Une révision bloquée derrière une ancienne en backoff** — colonne `fanout_outbox.retry_at` ; règle recopiée dans InMemory.
- [x] **6. Baux de flux fantômes renouvelés pour toujours** — `renew(replica, held)` ne prolonge que les baux ouverts localement.
- [x] **7. Une sync client filtrée avançait le curseur partagé** — `notify-client-core/src/sync.rs`.
- [x] **8. Passe de fond sans limite de temps, pas de keepalive TCP** — `PASS_TIMEOUT` 5 min dans `guarded` ; `tcp_keepalives_*`.
- [x] **9. `signed_for` ignorait l'âge de la présignature** — marge d'une heure avant la coupure du balayage.
- [x] **10. Migration du cache client non atomique** — une transaction `BEGIN IMMEDIATE` ; `busy_timeout` avant le passage en WAL, WAL réessayé (borné).
- [x] **11. Le TUI enfermait dans Tags/Actions** — une ligne vide quitte l'édition.
- [x] **12. Réconciliation inerte sur minuteur après un refus de suppressions** — `owed = refused > 0` (sans test : boucle construite seulement sur un vrai `PgStore`).
- [x] **13. SIGHUP : audit d'une politique retirée mais toujours appliquée** — politiques construites avant l'adoption, tout ou rien.
- [x] **14. Sinks plafonnés à ~20/s** — le drain enchaîne les lots tant qu'ils sont pleins.
- [x] **15. Silences jamais purgés, historique complet dans chaque événement** — `silences_for` filtré ; `purge_silences` dans la passe de purge.
- [x] **16. Avatar : course et fuite d'objet** — `set_avatar` atomique ; `upsert` ne réécrit plus l'avatar. Deux `PATCH /me` simultanés (ou un `PATCH` et le rafraîchissement des claims au login) ne s'écrasent plus : `UserRepository::replace` compare-and-swap, l'édition est réappliquée sur la relecture (3 essais, puis 409).
- [x] **17. 409 au premier login en parallèle** — relecture par `oidc_subject` sur conflit.
- [x] **18. (régression f2032ed) Canal rejoint invisible jusqu'à 60 s** — `Poke::User` à l'ajout de membre et au self-join ; le flux relit ses canaux. *Les changements par groupe ou par GitOps gardent le délai de 60 s.*
- [x] **19. Digests envoyés un par un** — 16 personnes à la fois.
- [x] **20. Desktop et CLI mélangeaient deux serveurs dans un cache** — le cache retient son serveur et refuse les autres.
- [x] Petits : relecture des accusés en une requête (`get_many`) ; détail web qui perdait les événements pendant le chargement ; doublons « nouveau » dans l'inbox ; TUI rafraîchi toutes les 20 s et desktop qui retente l'élection ; « jusqu'à demain 8 h » prend le fuseau du profil ; ligne `:token` hors historique ; `Bearer` insensible à la casse ; identifiants GitOps validés au chargement ; nouvel issuer signalé au SIGHUP ; pas de « prêt » sur erreur base au démarrage ; trois index redondants supprimés.
- [x] Trouvé en route : `persistence-pg` sans `build.rs`, donc une nouvelle migration n'entrait dans aucun binaire tant que rien d'autre ne changeait dans le crate (`VersionMissing`).

## Trouvé par k6 (2026-10-24)

Mesuré en local (16 cœurs, Postgres de `task dev:core`, un réplica, pool de 16) avec
`task perf:*`. Les seuils des scénarios sont les cibles : ils échouent tant que ces points
sont ouverts. Écriture seule : OK (150 publications/s, p95 9,5 ms, 0 erreur).

- [x] **K1. Chaque publication coûtait une lecture base par flux ouvert** — `notify-app/src/live.rs`, `http-actix/src/routes/stream.rs`, ADR-0030
  - 1 000 flux WebSocket : à 20 publications/s, `POST /notifications` passait de 9 ms à 600 ms et la livraison à ~1 s ; à 100/s, la livraison dérivait jusqu'à 20 s. Un poke réveillait chaque flux, qui relisait le journal pour lui seul (droits, settle, page, deux `count(*)` jetés, DND).
  - Fix : `LiveFeed`, un lecteur par réplica (`EventStore::tail`), diffuse chaque lot ; chaque flux filtre en mémoire et ne relit lui-même que pour rattraper (trou, retard, adhésion). Curseur d'appareil écrit au plus toutes les 30 s et à la fermeture.
  - Mesure, même run : 20/s → livraison p95 24 ms, publication p95 14 ms ; 100/s → livraison p95 57 ms (p99 299 ms), publication p95 34 ms, 18 M frames, 0 flux perdu. Tous les seuils de `perf:run:fanout` passent.
  - ~~Reste : chaque flux sérialise sa propre copie de chaque événement ; non mesuré à plusieurs réplicas.~~ Fait le 2026-10-25 : `LiveBatch.frames`, le JSON rendu une fois par réplica ; un flux ne recopie et ne re-sérialise qu'un événement que son DND rend muet. CPU serveur 345 → 320 s pour 18 M frames (−7 %, charges de test minuscules ; le gain croît avec la taille du contenu). Deux réplicas (`task perf:replica`, `PERF_REPLICAS=http://localhost:8081`) : livraison p95 39 ms sur chacun, le passage par le bus Postgres ne se voit pas ; 0 flux perdu.
- [x] **K2. L'inbox triait tout le canal à chaque appel** — `persistence-pg/src/notifications.rs` (`list`)
  - `channel_id = ANY ($1) … ORDER BY cursor_seq DESC LIMIT 51` ne parcourait pas `notifications_inbox_idx` dans l'ordre : seq scan + tri de tout le canal (40 000 lignes, 26 ms). 100 VU saturaient Postgres.
  - Fix : une page par canal (`unnest` + `LATERAL … LIMIT`) puis fusion ; 0,55 ms sur la même base. `perf:run:read` : 234 → 702 req/s, p95 inbox 448 → 72 ms, `/channels` 364 → 92 ms, `/sync` 773 → 244 ms ; tous les seuils passent. Test : `an_inbox_over_several_channels_pages_in_one_order`.
  - Laissé : `search` garde `ANY`, l'index GIN est le bon chemin pour un terme sélectif.
- [x] **K3. `/sync` parcourait le journal pour ses deux compteurs** — `persistence-pg/src/events.rs`, migration `20261025000000_event_count_indexes`
  - `expired_while_away` cherchait `kind = 'expired_unseen'` sans index : les 65 000 événements lus pour n'en trouver aucun (27 ms). `compacted` lisait 10 000 lignes du tas (15 ms).
  - Fix : deux index partiels (`expired_unseen` ; `revision` avec `INCLUDE (notification_id)`), le compte de compaction ne lit que les révisions (toujours adressées à un canal). 27 → 0,04 ms et 15 → 10 ms (balayage 1,6 ms, le reste est le `count(DISTINCT)` sur 10 000 lignes, seulement pour une fenêtre aussi longue). `perf:run:read` : p95 `/sync` 244 → 180 ms, 702 → 891 req/s.
  - Laissé : chaque `/sync` fait encore un upsert sur `principal_group_snapshots` (`remember_groups`) ; à sauter si groupes inchangés et vus il y a moins d'une minute, si la charge le demande.
