# weebo-notify — Design Document

> Plateforme de notifications riches, chiffrées (serveur de confiance, re-chiffrement ciblé vers les transports push tiers), multi-canaux et multi-appareils.
> Remplaçant de Gotify / ntfy pour la stack `weebo-si`.

---

## 1. Vision

Un serveur de notifications self-hosted qui traite la notification comme un **objet à cycle de vie** (et non un simple message fire-and-forget) : statut, mises à jour successives, image, actions, accusés de lecture, livraison différée avec avertissement de retard — chiffré là où un tiers peut lire : les relais push mobiles (modèle serveur de confiance, §6).

Trois personas cibles :

| Persona | Besoin |
|---|---|
| Sysadmin | 1 canal par cluster, notifications incident open/resolved, image d'état, CTA (runbook, silence, ack) |
| Développeur solo | Notification longue durée liée à un script : started → running → finished(status), boutons d'interaction |
| Équipe dev | Événements CI/CD (start/end) + messages sporadiques (MR ouverte), publiés par des jobs via jetons éphémères type GitHub/Forgejo Actions |

---

## 2. Modèle de domaine

### 2.1 Entités

```
Instance (single-tenant en v1)
   ├── User (Profile : display_name, avatar, timezone, locale — §2.4)
   │      └── Device (transport push + curseur de sync ; clé X25519 pour les devices mobiles)
   ├── ServiceAccount (identité machine nommée : API keys, memberships, curseur — §5.2bis)
   └── Channel (politique, membres : users & service accounts, ApiKeys)
          │
          └── Notification (id stable, révisions, état)
                 ├── Revision (payload, `seq` serveur monotone, author — le principal qui a publié, §5)
                 ├── Action (CTA)
                 └── Receipt (delivered / seen, par device/user)
```

**Single-tenant en v1, extensible ensuite** : une instance = un espace unique (users, canaux, policies) ; aucune API ni config n'expose de notion de tenant. Le schéma SQL porte néanmoins dès le départ une colonne `tenant_id` (valeur constante `default`) sur les tables racines : l'extension multi-tenant sera une migration additive (contraintes d'unicité re-scopées par tenant), pas une refonte.

### 2.2 Cycle de vie d'une notification

Une notification est **adressable et mutable** : le publisher crée une notification avec un `notification_id` (ou en fournit un idempotent via `external_id`), puis pousse des révisions.

```
open ⇄ working ──▶ closed(status) ──▶ ré-ouvrable (une révision peut repasser open/working)
                     status ∈ { success, failure, warning, info, cancelled }
```

- **Une notification à cycle de vie naît `open`** (au sens « encore active ») — pas d'état draft.
- **Fire-and-forget = publish sans champ `state`** : la notification est dite *simple*, créée directement `closed(status ?? "info")`, et son TTL court dès la création (§7.2bis). La présence du champ `state` est ce qui distingue formellement les deux types.
- `PUT /channels/{id}/notifications/{external_id}` (§8) crée ou met à jour (upsert idempotent) → parfait pour la CI : le job pousse `state=started` puis `state=finished, status=success` sur le même id.
- Chaque révision est un événement immuable ; le serveur lui attribue un **`seq` monotone à la réception** ; les clients affichent la dernière révision mais peuvent dérouler l'historique.
- **Concurrence : last-write-wins** — en cas de publishers concurrents (retries CI, jobs parallèles), l'état courant est celui du plus grand `seq` (ordre d'arrivée serveur). Pas de `If-Match` : la timeline conserve toutes les révisions, l'historique fait office d'audit.
- **Une notification `closed` reste inscriptible** : une révision poussée dessus est acceptée — soit elle amende la fermeture (nouveau `status`), soit elle **rouvre** la notification (`state=open|working`, cas typique du re-run CI sur le même `external_id`). À la ré-ouverture le TTL est de nouveau suspendu (il ne court qu'à l'état `closed`, §7.2bis) et repart de zéro à la fermeture suivante.
- TTL et priorité (`min`, `low`, `default`, `high`, `urgent`) par notification, avec défauts par canal.

### 2.3 Contenu riche (payload — en clair sur TLS, sealed box sur les push mobiles §6.2)

```jsonc
{
  "title": "cluster-ovh-1 · etcd degraded",
  "body": "markdown supporté",
  "state": "open",              // open | working | closed
  "status": "failure",           // sémantique de closed/open
  "progress": { "current": 3, "total": 10 },   // optionnel
  "image": { "kind": "attachment", "ref": "att_..." },  // ou data-uri ≤ 48 KiB (au-delà : attachment)
  "icon": "cluster",
  "actions": [
    { "type": "view",    "label": "Runbook", "url": "https://..." },
    { "type": "ack",     "label": "Ack", "key": "ack-incident" },
    { "type": "reply",   "label": "Comment", "placeholder": "Notes d'incident..." },
    { "type": "choice",  "label": "Escalade", "key": "escalate",
      "options": [ { "key": "oncall", "label": "Page on-call" }, { "key": "defer", "label": "Attendre" } ] },
    { "type": "silence", "label": "Silence 4h", "scope": "group", "duration": "4h" }
  ],
  "tags": ["etcd", "prod"],
  "group_key": "incident-4521"   // regroupement côté client
}
```

**Types d'action** (spec complète) :

| Type | Champs | Effet | Résultat |
|---|---|---|---|
| `view` | `label`, `url` (`http(s)` uniquement) | ouvre l'URL côté client | aucun (client only) |
| `ack` | `label`, `key` | acquittement **idempotent** — 1 par user et par `key`, re-cliquer = no-op | interaction |
| `reply` | `label`, `placeholder?` | saisie de texte libre | interaction |
| `choice` | `label`, `key`, `options[{key,label}]` | 1 option **exclusive** par user et par `key` — la dernière gagne (LWW, §2.2) | interaction |
| `silence` | `label`, `scope: "notification" \| "group"`, `duration?` | snooze **côté serveur** pour le user : plus de push ni de badge sur tous ses devices, pour la notification seule ou tout le `group_key`. Sans `duration` : scope `notification` = jusqu'à fermeture/expiration ; scope `group` = **`duration` requise** (pas de mute de groupe éternel implicite) | état par user (pas une interaction) |

Contraintes :
- **5 actions max** par notification (§8.2) ; l'ordre du tableau = ordre de priorité — les transports push mobiles n'affichent que les 2–3 premières (limite OS), le reste est accessible en ouvrant le client.
- `label` ≤ 40 caractères (strippé comme tout champ texte, cf. hygiène ci-dessous).
- À chaque révision, les actions sont **remplacées intégralement** (cohérent avec le LWW §2.2) ; les interactions déjà enregistrées restent liées à la notification, pas à la révision.

**Interactions internes** : `ack`, `reply` et `choice` ne déclenchent aucun appel HTTP sortant depuis le device — elles sont enregistrées côté serveur (`POST /notifications/{id}/interactions`) et **queryables par les services qui en ont besoin** (`GET /notifications/{id}/interactions`, ex. le script qui attend son `approve`). Seul `view` ouvre une URL côté client ; `silence` modifie l'état de livraison côté serveur (`POST /notifications/{id}/silence`, §8). La priorité `urgent` ne contourne pas les modes silencieux (pas d'entitlement critical alert iOS).

**Hygiène du contenu (anti-injection)** — la surface d'attaque n°1 d'un produit de notifications, contrat appliqué à l'ingestion et au rendu :
- Le `body` est du **markdown pur** : tout HTML brut est traité comme du texte (sanitize au rendu web §10.3, CSP stricte).
- Les **séquences d'échappement ANSI et caractères de contrôle** sont strippés de tous les champs texte (title, body, tags, labels) — protection des terminaux (TUI §9.2).
- Les URLs d'action `view` sont validées **à l'ingestion et avant ouverture côté client** : schémas `http`/`https` uniquement (jamais de `javascript:`, `file:` ou schéma custom vers `xdg-open`).

Les pièces jointes (images) sont uploadées vers un object store S3-compatible (**RustFS** en dev et en self-hosted par défaut, n'importe quel S3 en prod) via URL pré-signée ; la notification ne porte que la référence. Le bucket est **dans la frontière de confiance** au même titre que Postgres (§6.1) : strictement privé, presign = seul chemin d'accès, pas de chiffrement applicatif des attachments.

### 2.4 Profils (users & service accounts)

- Chaque **user** a un profil : `display_name`, **avatar**, `timezone` (sert de défaut aux quiet hours DND §7.4), `locale`. Initialisé depuis les claims OIDC (`name`, `picture`) au premier login, **surchargeable** dans Settings (§10.4) — un champ modifié localement n'est plus écrasé par les syncs OIDC suivants.
- **Avatar** : seul upload qui transite par le serveur (contrairement aux attachments §6.1) — ≤ 512 KiB, `png`/`jpeg`/`webp` statiques, **redimensionné en 256×256 à l'ingestion**, stocké dans le bucket, servi via URL authentifiée. `PUT /users/me/avatar` (§8). Décodage = surface d'attaque (§15) : lib sûre (image-rs), plafond de dimensions vérifié **avant** décodage complet (anti-bombe de décompression), formats animés refusés, et **toutes les métadonnées strippées** au resize — un avatar pris au téléphone embarque des coordonnées GPS EXIF.
- **Pas d'endpoint de lecture des profils d'autrui** : l'information n'arrive qu'**embarquée** là où elle est légitime (`author`, listes de membres, receipts) — c'est ce qui garantit mécaniquement la règle « visible uniquement via un canal commun » ; ne pas ajouter de `GET /users/{id}/profile` plus tard sans repasser par cette analyse.
- Les **service accounts** (§5.2bis) portent le même profil minimal (`display_name`, avatar, `description`) — un bot avec une icône reconnaissable dans la timeline vaut mieux qu'un id brut.
- Le profil s'affiche partout où un principal apparaît : **`author`** des révisions et interactions (timeline §10.3, SSE §8.1), « vu par 3/5 » (HoverCard), liste des membres. Les profils ne sont lisibles que par les membres d'au moins un canal commun.

---

## 3. Socle projet — template `weebo-base`

Le repo est bootstrappé depuis [`batleforc/weebo-base`](https://github.com/batleforc/weebo-base), le template commun à tous les projets weebo. On **hérite de ses conventions telles quelles** et on ne fait qu'étendre ; toute amélioration générique remonte dans le template, pas dans ce repo.

### 3.1 Ce que le template apporte

| Élément | Rôle | Adaptation weebo-notify |
|---|---|---|
| `mise.toml` | Outillage versionné (cocogitto, node 24, rust, task, gitleaks, claude, rtk) | + `sqlx-cli`, `cargo-nextest`, `cargo-llvm-cov` (coverage §3.2), `awscli` (seed bucket RustFS), générateur de clients OpenAPI (Rust : progenitor ou openapi-generator ; TS : idem — **à figer au bootstrap**, §3.2 `contract`) |
| `Taskfile.yaml` + `.tasks/` | Point d'entrée unique des commandes (`task init`, `task recu`, `task lint`) | `recu` = export de la spec OpenAPI (utoipa) + régénération des **clients dérivés de la spec** (TS pour le web, Rust `notify-client-api`), spec **et** clients commités ; `lint` = `cargo fmt && cargo clippy` + lint web ; ajout du namespace `dev:*` (§12) |
| `cog.toml` + `.hooks/` | Conventional commits (cocogitto), changelog, hooks `commit-msg` (`cog verify`/`cog check`) et `pre-commit` (`task recu`, `task lint`, `gitleaks`, EOL/whitespace) | Champs `repository`/`owner` du changelog pointés sur `weebo-notify` |
| `devfile.yaml` | Workspace Eclipse Che, `parent` → [`WeeboDevImage`](https://github.com/batleforc/WeeboDevImage) `che-min-mise` | Étendu avec les sidecars de l'env de dev (§12) |
| `.forgejo/` / `.github/` | Double forge : Forgejo = forge principale (CI), GitHub = miroir | Workflows CI (§3.2) |
| `.rtk/`, `CLAUDE.md`, `.claude/` | Outillage token-optimisé + instructions agent | Conservés |

Workflow d'initialisation inchangé :

```bash
# après création du repo depuis le template
task init          # mise upgrade + cog install-hook -a -o + task dev:setup (§12.7)
```

### 3.2 CI (Forgejo Actions, miroir GitHub)

- `check` : `task lint` + `cargo llvm-cov nextest` (tests **et** coverage en un seul run — pas de double exécution de la suite) + tests web — sur chaque push/PR.
- `contract` : `task recu` puis `git diff --exit-code` — la spec OpenAPI et les clients générés (TS + Rust) étant commités, ils ne peuvent pas dériver du code serveur ; un oubli de régénération casse la PR.
- `e2e` : testcontainers (Postgres + RustFS) contre le binaire réel, **le provider OIDC étant in-process** (crate `oidc-mock` du workspace, ADR-0014 — aucune image à puller, aucun registry qui puisse être down) — sur PR vers `main` ; inclut la suite `critical` (§3.4), bloquante. Le flow OIDC complet (code + PKCE, JWKS) est donc joué sur chaque PR, contre le mock.
- `oidc-compat` : **la compat IdP est un contrat testé, pas une promesse** (produit self-hosted — les utilisateurs viendront avec Keycloak, Authelia, Dex…). Depuis ADR-0014 ce job est **le seul endroit où le projet rencontre une lecture d'OIDC qui n'est pas la sienne** : le mock du quotidien est écrit ici, donc il ne peut pas nous surprendre — c'est précisément sa faiblesse. La même suite `auth`, **paramétrée par issuer** (fixture par provider : issuer, credentials, attentes de claims — sélection par env, cohérent §11), est rejouée contre **Dex** (OIDC minimal strict — couvre aussi le cas « IdP sans claim `groups` », T29) et **Authentik** (blueprint §12.5 — comportements réels : rotation du refresh token, logout, groupes). Les **mots de passe Dex sont générés au boot du job** (aléatoires, hashés bcrypt dans une config rendue à l'exécution depuis un template commité, transmis à la suite par env — jamais commités) : Dex est CI-only, aucun besoin d'ergonomie locale ne justifie un mot de passe fixe, contrairement à `alice`/`bob` du mock (§12.5). Déclenchée sur les PR qui touchent l'auth (paths : adapter `oidc/`, mock `oidc-mock/`, middleware principal, use cases auth), en nightly, et dans le gate de release (§3.2bis) — pas sur chaque PR de front (Authentik ≈ 1 min de boot).
- `release` : déclenchée par le push d'un tag `v*`, entièrement automatique, pre-releases comprises — section dédiée §3.2bis.
- La CI publie elle-même ses statuts vers weebo-notify via la fédération JWT (§5.3) dès M4 — dogfooding.

**Code coverage** (mesuré dans le job `check`, commande locale `task cover`) :

- **Rust** : `cargo llvm-cov nextest` (source-based, précision branche/région) ; **web** : `vitest run --coverage` (provider v8). Les deux sortent du LCOV, fusionné en un rapport unique.
- **Périmètre honnête** : le code généré est exclu de la mesure (`notify-client-api`, client TS) ; les adapters I/O sont mesurés via les E2E testcontainers mais **le gate ne porte que sur `notify-domain` + `notify-app`** — logique pure, testable in-memory (§4), donc exigence haute : **≥ 90 %**.
- **Anti-régression plutôt que cible globale** : sur PR, résumé posté en commentaire ; échec si la **diff coverage** des lignes touchées < 80 % ou si la couverture domaine/app **baisse** (ratchet). Pas de gate sur le chiffre global : un seuil unique se game (tests sans assertions) et pénalise injustement le code d'adaptation.
- **Self-hosted** : aucune dépendance codecov.io — rapport HTML en artefact CI, badge servi depuis le job Forgejo.

### 3.2bis Release — tout part d'un tag

Règle : **poser le tag est la seule action humaine**. `task release` (localement, ou workflow manuel Forgejo) exécute `cog bump --auto` — version calculée depuis les conventional commits, changelog généré, versions synchronisées par hook cocogitto (`Cargo.toml` workspace, `web/package.json`, `tauri.conf.json` : **une seule version pour tout le produit**) — puis pousse le tag `v*`. Le push du tag déclenche tout le reste, sans aucune intervention :

1. **Gate** : la release rejoue `check` + `contract` + `e2e` (suite `critical` §3.4 incluse) sur le commit taggé — un tag ne dispense jamais des gates de PR.
2. **Build des artefacts** (matrice alignée sur §10bis.6) :
   - image serveur **multi-arch** (amd64/arm64), signée **cosign** + provenance **Rekor** ;
   - binaires **CLI/TUI** (linux amd64/arm64, macOS, Windows) en archives + `SHA256SUMS` signé ;
   - packages **desktop Tauri** (`.deb`/`.rpm`/AppImage/`.dmg`/`.msi`, §10bis.3) + manifest updater signé minisign ;
   - **APK Android** signé (dès M6), build iOS → TestFlight (dès M7, runner macOS §10bis.5).
3. **Publication atomique** : la release Forgejo est créée en *draft*, les artefacts uploadés, les notes remplies depuis le changelog cocogitto, puis publiée **d'un coup** — jamais de release à moitié vide ; le miroir GitHub reçoit la même release.
4. **Post-publication** : le manifest updater du canal concerné est mis à jour (le point qui rend l'update desktop effective), et la CI notifie le canal `ci-infra` de weebo-notify via la fédération §5.3 (dogfooding).

**Pre-releases** — tag SemVer `vX.Y.Z-rc.N` (`task release -- --pre rc`) :

- **Même pipeline, mêmes artefacts, mêmes signatures** : une RC est un produit complet, pas un build au rabais — c'est ce qui permet de la promouvoir sans re-valider.
- Marquée **pre-release** sur la forge ; l'image est poussée avec son tag exact, **`latest` ne bouge pas**.
- **Canaux updater desktop** : `stable` (défaut) ne sert que les tags fermes ; `beta` (opt-in dans Settings, §10bis.3) sert le dernier tag, RC comprises. Obtainium/F-Droid ne voient que les stables.
- **Promotion** : une RC validée n'est pas « re-buildée en stable » — on pose `vX.Y.Z` sur le **même commit** et le pipeline rejoue à l'identique.

Garanties :

- **Un tag ne bouge jamais** : release ratée = correctif + nouveau tag (`rc` suivant ou patch), jamais de re-tag.
- **Pipeline rejouable** : chaque job est idempotent — relancer un run échoué reprend sans dupliquer ni corrompre ce qui a déjà été publié.

### 3.3 ADRs — convention à remonter dans `weebo-base`

Reco propagée au template (utile à tous les projets qui en dérivent) :

- `docs/adr/NNNN-titre-kebab.md`, numérotation croissante, ADRs **immuables** : on ne réécrit pas un ADR accepté, on le remplace (`Remplacé par : ADR-00NN`).
- Format MADR minimal, une page max :

```markdown
# ADR-0003 — Sealed box par device plutôt que MLS
Statut : accepté (2026-08-01) · Remplace : — · Remplacé par : —

## Contexte
## Décision
## Conséquences (positives / négatives / dette assumée)
```

- Outillage côté template : `task adr -- "titre"` crée le fichier numéroté depuis un gabarit `.tasks/adr.md` ; le README pointe vers `docs/adr/`.
- Règle d'usage : un ADR quand la décision est **coûteuse à inverser** ou a fait débat — pas pour chaque choix de lib.

Premiers ADRs weebo-notify (à extraire de ce document au bootstrap) : SSE transport de référence (vs WS), RustFS comme S3 dev/self-hosted, sealed box par device (vs MLS), shadcn-vue (vs lib de composants), receipts non signés, single-tenant v1, pas de chiffrement applicatif au repos (disques + backups chiffrés côté infra), eventbus-pg par défaut (redis en option HA).

### 3.4 Scénarios de test critiques

Suite taggée `critical` : bloquante sur PR, jamais skippée, et chaque bugfix y ajoute son scénario de régression. Les scénarios T1–T6 sont des tests unitaires du domaine (in-memory, §4) ; le reste vit en intégration/E2E (testcontainers).

**Domaine — cycle de vie & contenu**
- **T1 · Naissance** : publish sans `state` → notification créée `closed(info)`, TTL court dès la création ; avec `state` → née `open`, inexpirable tant que non fermée (§2.2, §7.2bis).
- **T2 · Réouverture** : révision sur une `closed` → amende ou rouvre ; TTL re-suspendu, repart de zéro à la re-fermeture.
- **T3 · LWW** : publishers concurrents (retries, ordre d'arrivée mélangé) → état courant = plus grand `seq`, historique intégral conservé, aucune révision perdue.
- **T4 · Actions** : remplacement intégral des actions à chaque révision ; `ack` idempotent (2ᵉ clic même user/key = no-op) ; `choice` exclusif LWW par user ; `view` avec `javascript:`/`file:` → rejeté à l'ingestion.
- **T5 · Hygiène** : title/body contenant séquences ANSI (`\x1b[2J`), caractères de contrôle ou HTML brut → strippés/inertes en sortie (web, TUI, webhook).
- **T6 · TTL** : `ttl > max_ttl` → clampé + signalé dans la réponse ; `ttl = 0` refusé sans policy explicite ; 1 001ᵉ révision → `422`, notification intacte.

**Sync & temps réel**
- **T7 · Reprise curseur** : déconnexion pendant N publications → `GET /sync?cursor` rejoue exactement N événements, ordonnés, zéro doublon ; le même curseur fonctionne en `Last-Event-ID` (§8.1).
- **T8 · Compaction** : 40 révisions hors-ligne → 1 entrée au sync (dernière révision + résumé du cycle).
- **T9 · Expiration hors-ligne** : message expiré non livré → absent du backlog, compteur « N expirées » exact par canal.
- **T10 · Retard** : `delivered_at − created_at > seuil` → flag retard ; différée par DND (`deferred_by_dnd`) → pas de flag (§7.4).

**AuthN/AuthZ**
- **T11 · Token exchange** : `aud` étranger → refus ; `repo:weebo-evil/x` contre policy `repo:weebo/*` → refus (glob ancré) ; même `jti` rejoué → refus ; policy de canal prioritaire sur la globale (§5.3).
- **T12 · API keys** : clé publish-only ne peut ni subscribe ni manage ; clé expirée → `401` code dédié ; le secret n'est re-lisible par aucun endpoint après création.
- **T13 · OIDC** : mapping `groups` → accès aux canaux managed conforme à la config ; user hors groupe → `403` ; token expiré/mal signé → `401`.

**Quotas & robustesse**
- **T14 · Rate limit** : 61ᵉ publish/min d'une clé → `429` + `Retry-After` ; les autres publishers du canal ne sont pas affectés.
- **T15 · Tailles** : payload 65 KiB → `413` ; presign demandé à 26 MiB → refusé.
- **T16 · Idempotence** : `Idempotency-Key` rejoué → réponse identique, une seule notification créée ; `PUT external_id` rejoué → une seule notification, deux révisions.

**Distribution**
- **T17 · Webhook sink** : signature `t`/`v1` vérifiable par le consommateur de référence ; replay à `t − 6 min` → rejeté ; endpoint down → retries backoff → DLQ à 24 h → requeue manuel fonctionne (§7.1bis).
- **T18 · Sealed box mobile** : device mobile enrôlé → le blob push est indéchiffrable sans sa clé privée (test avec une autre clé → échec) ; révocation → plus aucun push, curseur invalidé (§6.2).
- **T19 · DND** : pendant quiet hours → zéro push OS, inbox alimentée, SSE intact ; sortie → **un** digest ; canal en exception → push passe ; `urgent` sans bypass → bloqué (§7.4).
- **T20 · Silence** : `silence` sur un `group_key` → plus de push/badge pour ce user sur ce groupe, les autres membres ne sont pas affectés (§2.3).

**Ops**
- **T21 · Réconciliation GitOps** : canal retiré de la config → archivé/supprimé selon `on_removed` ; drift (TTL modifié en base) corrigé au SIGHUP ; canal `dynamic` jamais touché (§11). **Trois clauses, et le test qui porte ce nom n'en couvrait qu'une** jusqu'à M4duodetricies — celle du canal `dynamic` ; le SIGHUP n'existait pas du tout. Un nom de test est une affirmation sur ce qu'il vérifie, et personne ne la relit contre le texte cité.
- **T22 · Leader election** : kill du leader → reprise purge/réconciliation par un autre replica ; jamais deux purges concurrentes (advisory lock testé sous contention).
- **T23 · Config** : test généré qui itère le schéma complet et vérifie que chaque clé est surchargeable par `WNK_…` (§11).
- **T24 · Migrations** : boot sur base vide → schéma complet ; re-boot → idempotent, zéro migration rejouée.
- **T25 · Service accounts** : un SA `subscribe` ne peut ni publier ni manage ; ses révisions portent le bon `author` (timeline + SSE) ; désactivation → clés révoquées et SSE coupé immédiatement ; jamais compté dans « vu par x/y » (§5.2bis).
- **T26 · Preference & profils** : l'événement `preference` (§8.1) n'est reçu que par les connexions du user concerné — jamais par un autre user du même canal ; le profil d'un user n'apparaît (author, membres, receipts) qu'aux membres d'au moins un canal commun (§2.4) ; l'attribution d'une clé suit son propriétaire (SA → SA, user → user, canal → la clé, §5.2).

**Clients locaux & recherche**
- **T27 · Recherche scopée** : `GET /search` (§8) ne retourne jamais une notification d'un canal dont le principal n'est pas membre (testé avec deux users et un service account) ; une notification expirée/supprimée (§7.2bis) disparaît des résultats.
- **T28 · Store partagé** : deux frontales sur le même profil (§9.3bis) → une seule tient le SSE (lock) ; kill -9 du détenteur → l'autre reprend le flux sans trou ni doublon de curseur ; suppression du store → re-sync complet, état final identique.

**Compat IdP (matrice `oidc-compat`, §3.2)**
- **T29 · Matrice OIDC** : la suite auth — code + PKCE headless, validation JWKS + **rotation des clés de signature** (le mock maison expose `POST /admin/rotate-keys` pour la déclencher à la demande, ADR-0014), mapping `groups` → canaux managés, token expiré/mal signé/mauvais issuer → `401` — passe à l'identique sur le mock maison, Dex et Authentik. Cas propre à Dex (pas de claim `groups`) : **dégradation propre** — l'user n'accède qu'à ses bindings individuels `oidc_sub`/`email` (§11), jamais d'accès par défaut ni de crash.

**Rôle plateforme**
- **T30 · Frontière admin** (§5.4) : un `platform-admin` **non membre** d'un canal — inventaire et quotas accessibles, mais `GET /channels/{id}/notifications` → `403` et aucun événement de ce canal dans son SSE ; ses opérations (requeue DLQ, révocation, self-join) apparaissent dans l'audit log ; un user sans le rôle → `403` sur tout `/admin/*` ; aucun chemin API ne permet de s'accorder le rôle (GitOps only).

---

## 4. Architecture hexagonale

Même philosophie que `weebo-runner` : domaine pur, ports explicites, adapters interchangeables, composition root unique.

```
crates/
├── notify-domain/          # Entités, invariants, state machine des notifications. Zéro dépendance I/O.
├── notify-ports/           # Traits (async) :
│   │                       #   NotificationRepository, ChannelRepository, DeviceRepository,
│   │                       #   ReceiptRepository, PushTransport, AttachmentStore, SinkDispatcher,
│   │                       #   TokenVerifier (OIDC/JWT), Clock, EventBus, LeaderElection, AuditLog
├── notify-app/             # Use cases : PublishNotification, UpdateNotification,
│   │                       #   RegisterDevice, JoinChannel, AckReceipt, MintCiToken, ...
├── notify-adapters/
│   ├── http-actix/         # REST + SSE/WS, Utoipa (OpenAPI 3.1 générée, servie sur /api/docs)
│   ├── persistence-pg/     # sqlx, migrations versionnées
│   ├── push-webpush/       # RFC 8030/8291 (VAPID)
│   ├── push-unifiedpush/   # Android self-hosted
│   ├── push-apns/          # iOS (token-based auth .p8)
│   ├── attachments-s3/     # S3-compatible (RustFS en dev/self-hosted)
│   ├── sinks-webhook/      # POST JSON signé HMAC + le vérificateur de référence (§7.1bis)
│   ├── sinks-email/        # SMTP (lettre/rustls) — une révision, un mail en clair
│   ├── eventbus-pg/        # LISTEN/NOTIFY (défaut) — pokes (ids) uniquement, limite 8 Ko de NOTIFY oblige
│   ├── eventbus-redis/     # Pub/Sub Redis/Valkey — option HA/scale-out (§11), même contrat de pokes
│   └── oidc/               # Découverte, JWKS cache, validation (Authentik + issuers CI fédérés)
├── notify-server/          # Composition root : config → wiring → run. Binaire unique.
├── notify-client-api/      # Client Rust généré depuis la spec OpenAPI (commité, régénéré par `task recu`)
├── notify-cli/             # clap + TUI ratatui (consomme notify-client-api + notify-client-core)
└── notify-client-core/     # Logique partagée CLI/Tauri : cache local, sync ; crypto device (X25519) sur cibles mobiles uniquement
```

Règles :
- `notify-domain` et `notify-app` testables sans I/O (adapters in-memory dans `notify-ports/test-utils`).
- Tests E2E : testcontainers (Postgres + RustFS) contre le binaire réel.
- Horizontalement scalable : serveur stateless, état dans PG/S3, fan-out via EventBus.

---

## 5. AuthN / AuthZ

Quatre identités, un seul middleware d'entrée qui produit un **`Principal` typé** du domaine — `User` (OIDC, §5.1) | `ServiceAccount` (API key possédée, §5.2bis) | `ChannelKey` (API key de canal, anonyme, §5.2) | `CiJob` (fédération, §5.3). Chaque endpoint déclare dans l'OpenAPI quels principals il accepte, et le champ **`author`** (§2.1) référence toujours le principal à l'origine d'une révision ou d'une interaction :

### 5.1 OIDC (humains) — modèle 100 % token, zéro session serveur
- Authorization Code + PKCE contre Authentik, **porté par la SPA** : le front obtient access + refresh tokens et appelle l'API en `Authorization: Bearer`. Le serveur valide en pur JWKS (mappe `groups` → rôles) et ne stocke **aucune session** — rien à répliquer entre replicas, la HA reste triviale (§11 `[ha]`).
- Pas de cookie de session → **pas de surface CSRF** ; CORS fermé par défaut (origins du front déclarés en config).
- Refresh silencieux par **rotation du refresh token** (Authentik) ; tokens gardés hors du localStorage (mémoire + refresh piloté par le service worker). *Où vit réellement le refresh token* : **ADR-0020** (qui supersède ADR-0016). Le plan « le service worker le prendra » ne marche pas, les trois options ont été pesées, et **le jeton reste en `sessionStorage`** — risque accepté, avec ses quatre atténuations et ses conditions de réouverture écrites. Le « zéro session serveur » de cette section est ce que ce choix préserve, et la CSP d'ADR-0019 est devenue le contrôle porteur à sa place (D9).

### 5.2 API keys (machines simples)
- **Par utilisateur** (agit au nom de l'utilisateur), **par canal** (publish-only par défaut, anonyme) ou **par service account** (§5.2bis — la forme recommandée pour tout bot durable).
- Règle d'usage : la clé par utilisateur sert au scripting personnel (dotfiles, cron perso) ; **un bot d'équipe ne tourne jamais sur la clé d'un humain** — offboarder la personne ne doit pas tuer le bot.
- Format `wnk_<prefix>_<secret>` ; seul le hash (argon2id) est stocké ; préfixe indexé pour lookup O(1).
- Scopes : `publish`, `subscribe`, `manage`, restreints à une liste de canaux.
- **`expires_at` optionnel** à la création + `last_used` tracé (exposés via `GET /apikeys`, §8) ; clé expirée → `401` avec `code` dédié — permet des politiques de rotation sans les imposer.
- **Attribution (`author`, §2.1)** : une clé rattachée à un **service account** publie en tant que ce SA ; une clé **utilisateur** publie en tant que ce user ; une clé **de canal** (anonyme) est attribuée à la clé elle-même — affiché « via clé `wnk_ci4x` du canal ci-infra ». Quand l'attribution compte, préférer un service account (§5.2bis).

### 5.2bis Service accounts (bots)

Identité machine **nommée et durable**, principal de première classe :

- Créés en **GitOps** (`[[service_accounts]]`, §11 — réconciliés comme les canaux managés) ou via API/UI (marqués `dynamic`).
- Possèdent leurs **API keys** (mêmes propriétés que §5.2 : hash argon2id, scopes, `expires_at`, `last_used`), sont **membres de canaux** avec scopes exactement comme un user, et peuvent tenir une connexion SSE + un curseur de sync (device logique, sans transport push).
- Apparaissent comme **`author`** des révisions/interactions (timeline, SSE §8.1) et dans l'audit log (§13) — « publié par `bot-argocd` », pas « une clé du canal ».
- Pas de login OIDC, pas de DND ni de devices push ; **jamais comptés dans les receipts** « vu par 3/5 » (§7.3) — un bot qui a lu n'est pas un humain informé.
- Cas d'usage types : le service qui poll les interactions `approve` (§2.3), ArgoCD qui publie l'état de ses syncs, le futur bridge Matrix (§7.1bis).

### 5.3 Fédération JWT type GitHub/Forgejo Actions (CI/CD)
Aucun secret longue durée dans la CI :

1. L'admin déclare un **trust policy** (GitOps, TOML) : issuer OIDC (ex. Forgejo Actions), contraintes de claims (`repository`, `ref`, `workflow`), droits accordés (canaux + scopes).
2. Le job CI obtient son ID token natif et appelle `POST /auth/token/exchange`.
3. Le serveur valide le token contre le JWKS de l'issuer, vérifie la policy, émet un **jeton d'accès signé (Ed25519), TTL ≤ 15 min**, audience `weebo-notify`.

Durcissement de l'échange :
- L'**`aud` du token CI entrant** doit désigner weebo-notify — un token émis pour un autre service est refusé même si issuer et subject matchent.
- Les wildcards de `subject` sont matchés **ancrés** (glob compilé sur la chaîne entière) : `repo:weebo/*` ne matche jamais `repo:weebo-evil/x`.
- Le **`jti`** du token entrant est mémorisé jusqu'à son `exp` : un même ID token ne s'échange qu'une seule fois (anti-replay), le tout sous le rate limit §8.2.

```toml
[[trust_policy]]
issuer  = "https://forgejo.weebo.poc"
subject = "repo:weebo/infra:ref:refs/heads/main"
channels = ["ci-infra"]
scopes   = ["publish"]
```

### 5.4 Rôle plateforme (`platform-admin`)

Un rôle **instance-wide**, orthogonal aux scopes de canal (§5.2 — `manage` administre *un* canal, ce rôle administre *l'instance*) :

- **Déclaré en GitOps uniquement** : `[admin] oidc_groups` (§11) — aucune API ne permet de s'auto-promouvoir. Réservé aux **users OIDC** : un service account reste scoped à ses canaux, un bot n'administre pas la plateforme.
- **Métadonnées et opérations, jamais le contenu** : le rôle donne l'inventaire (canaux, membres, devices, clés, quotas), les opérations (requeue DLQ §7.1bis, révocations, réconciliation §11) et l'audit log (§13) — mais **pas la lecture des notifications** d'un canal dont l'admin n'est pas membre. Le membership reste la seule porte d'accès au contenu (cohérent avec §2.4) ; un admin peut s'ajouter à un canal, et ce self-join est **tracé dans l'audit log**.
- Toute action portée par ce rôle est journalisée (§13) ; il est consommé par la **console plateforme** (§10.4bis) et les endpoints `/api/v1/admin/*` (§8).

---

## 6. Chiffrement (serveur de confiance, re-chiffrement ciblé)

**Modèle de menace** : le serveur est **de confiance** et peut lire le contenu (nécessaire pour la compaction, la recherche, les policies). Ce qu'on protège applicativement : le contenu **en transit hors de notre infra** (relais push tiers : APNs, distributeurs UnifiedPush, push services navigateur) — personne d'autre que le serveur et les devices du canal ne doit pouvoir lire « le cluster est out ». **Au repos, rien d'applicatif** : disques et backups sont chiffrés au niveau infra (§6.1).

**La frontière de confiance est le cluster** : TLS peut se terminer à l'ingress ; le trafic interne (ingress→pod, pod→PG, pod→bucket) circule en clair applicatif à l'intérieur de cette frontière — son durcissement (network policies, mesh éventuel) relève du hardening plan, pas de crypto applicative supplémentaire.

### 6.1 Ingestion & repos

- **Publication** : payload JSON en clair sur TLS (curl pur, zéro friction) — pas de couche crypto maison côté publisher : si le réseau n'inspire pas confiance, la réponse est TLS + certificate pinning, pas un chiffrement applicatif redondant (cohérent avec la CLI zéro-crypto, §9.3).
- **Au repos : aucun chiffrement applicatif** (décision). Postgres comme le bucket sont dans la frontière de confiance (§6) ; la confidentialité au repos est assurée par l'infra — **disques chiffrés et backups chiffrés** par l'outillage de backup (hardening plan). Bénéfices : requêtes, compaction, recherche et purge sans couche crypto ; zéro gestion de clés applicative (pas de KEK/DEK, pas de rotation, pas de runbook). Si une exigence de conformité l'impose un jour, ça se réintroduit derrière un port `KeyProvider` sans toucher au domaine.
- **Attachments (trust the bucket)** : bucket strictement privé (zéro accès anonyme), presign = seul chemin d'accès (TTL court, `content-length-range` et content-type imposés) ; l'upload presigné va directement du client au bucket sans transiter par le serveur.

### 6.2 Livraison : re-chiffrement uniquement là où un tiers porte le payload

Principe : la sealed box n'existe que sur les chemins où un **relais tiers** transporte le payload. Chemin par chemin :

| Chemin | Relais tiers ? | Protection | Clé device applicative |
|---|---|---|---|
| SSE / WS / sync (web, desktop, CLI) | Non — TLS direct vers notre serveur | TLS | **Aucune** |
| Web Push (PWA) | Push service navigateur | **RFC 8291** : chiffré e2e serveur→navigateur, clés gérées par le navigateur lui-même (jamais exposées au JS) | **Aucune** |
| UnifiedPush (Android) | Distributeur UP | **Sealed box X25519** (XChaCha20-Poly1305) | X25519 (Android Keystore) |
| APNs (iOS) | Apple | **Sealed box X25519** | X25519 (Keychain, protection matérielle — §10bis.5) |

- Seuls les **devices mobiles** génèrent une paire X25519 à l'enrôlement (keystore matériel) et publient leur clé publique (`POST /devices`). À la livraison, le serveur re-chiffre le payload vers la clé publique de chaque device mobile destinataire avant de le remettre au transport push.
- L'enrôlement d'un device web/desktop/CLI se réduit à l'enregistrement de son transport (subscription Web Push le cas échéant) et de son curseur de sync — **zéro crypto applicative côté client**.
- Onboarding trivial : rejoindre un canal = décision serveur (membership), aucun échange de clés entre clients ni membre en ligne requis. Révocation d'un device = suppression de ses subscriptions push, de son curseur, et de sa clé publique s'il en a une — effet immédiat.

### 6.3 Push

- **Web Push** : payload applicatif en clair, protégé par le seul chiffrement RFC 8291 (suffisant : le push service ne peut pas lire, et aucune clé applicative n'existe côté navigateur — c'est le navigateur qui déchiffre avant de réveiller le service worker).
- **Mobile** : le blob push contient `{channel_id, notification_id, ciphertext_device}` (ou un simple « poke » si trop gros, le client fetch ensuite via TLS).
- iOS : **Notification Service Extension** (Swift, embarquée dans l'app Tauri) qui déchiffre localement avec la clé device avant affichage.
- Android/UnifiedPush : déchiffrement dans le service de réception.

---

## 7. Distribution & offline

### 7.1 Transports (choix)

| Cible | Transport temps réel | Push OS |
|---|---|---|
| PWA / navigateur | **SSE** (app ouverte) | **Web Push VAPID** |
| Desktop (Tauri) | **SSE** | notifs natives OS |
| Android (Tauri) | — | **UnifiedPush** (self-hosted : le serveur expose le endpoint push, le distributeur vit côté device — système ou embarqué dans l'app, §10bis.4) ; FCM en fallback optionnel |
| iOS (Tauri) | — | **APNs** (incontournable) |
| CLI / TUI | **SSE** (WebSocket disponible en option) | — |

Le SSE est le transport temps réel de référence (simple, proxy-friendly, reconnect natif via `Last-Event-ID` = curseur de sync) ; un endpoint WebSocket reste exposé pour les clients qui le préfèrent. SSE/WS étant du TLS direct vers notre serveur, le payload y circule **en clair applicatif** pour tous les clients (§6.2) — un `notify watch` éphémère dans un pod de debug fonctionne sans aucun enrôlement crypto.

### 7.1bis Sinks sortants (v1)

Un canal peut, en plus des devices, router vers des **sinks sortants** déclarés dans sa config GitOps : `webhook` (POST JSON signé HMAC), `email` (SMTP), extensible via le port `SinkDispatcher` (Matrix, Slack... adapters futurs). Les sinks reçoivent le payload en clair (le serveur est de confiance).

**Contrat webhook** :
- Body : l'événement au même schéma que le flux SSE (§8.1, event `revision`), en clair.
- Headers : `X-Weebo-Event` (type d'événement), `X-Weebo-Delivery` (identifiant unique et **stable à travers les retries** — clé d'idempotence côté consommateur ; un ULID `dlv_…` et non un UUID, cf. M4bis), `X-Weebo-Signature: t=<unix_ts>,v1=<hex(hmac_sha256(secret, t + "." + body))>`.
- Anti-replay : la signature couvre le timestamp ; le consommateur rejette si `|now − t| > 5 min`.
- Retries : backoff exponentiel 30 s → 1 h, abandon après 24 h → **DLQ** (métriquée §13, requeue manuel via `POST /admin/dlq/{id}/requeue` §8 — UI dans la console plateforme §10.4bis).
- **Une publication met en file, elle n'envoie pas** : le consommateur est le serveur de quelqu'un d'autre, et mettre sa latence devant la réponse d'un publisher ferait rougir un pipeline pour une raison qui ne le concerne pas. Le leader draine (§11) toutes les 5 s ; c'est ce délai, et non un aller-retour, qui sépare une publication de sa première tentative.
- **Le paiement de l'at-least-once** : une livraison est *baillée* et non retirée à la prise, donc un leader tué en pleine tentative la laisse due plutôt que perdue — au pire un doublon, ce que `X-Weebo-Delivery` existe précisément pour rendre supportable.

L'identité d'un sink est **dérivée de sa destination** (`webhook:https://…`, `email:…`) et la file ne porte jamais sa clé HMAC : le secret est résolu depuis la config au moment de l'envoi (ADR-0018). Trois conséquences directes — le secret reste hors des dumps, une rotation s'applique à ce qui est déjà en file, et un sink retiré de la config cesse d'être réessayé.

Rationale : UnifiedPush garde la boucle 100 % self-hosted sur Android (le serveur expose lui-même le endpoint push UnifiedPush, le distributeur vivant côté device §10bis.4) ; APNs est le seul canal fiable iOS ; Web Push couvre PWA et desktop navigateur sans dépendance tierce. Le endpoint push UnifiedPush embarqué — porte d'entrée publique supplémentaire — est authentifié par **token opaque par device** (révoqué avec lui, §8 `DELETE /devices`) et soumis aux quotas §8.2.

### 7.2 Store & forward + retard

- Toute notification est persistée ; chaque device a un **curseur de sync**.
- Au retour en ligne : `GET /sync?cursor=...` → backlog ordonné. Le client marque visuellement toute notification dont `delivered_at - created_at > seuil` avec un badge **« reçue en retard (+2h13) »** — le serveur fournit les deux timestamps, l'affichage est côté client.
- Les notifications `closed` écrasées par leurs révisions sont livrées compactées (dernière révision + résumé du cycle) pour éviter le spam au retour.

### 7.2bis TTL & rétention

- **Point de départ du TTL selon le type** :
  - Notification **avec cycle de vie** (champ `state` fourni au publish, §2.2) : le TTL ne démarre qu'à la **fermeture** (`closed`) ou à la suppression explicite. Une notification `open`/`working` ne peut pas expirer — un incident long reste visible jusqu'à sa résolution.
  - Notification **simple** (publish sans champ `state`, §2.2 — créée `closed`) : le TTL s'applique **dès la création**.
- **TTL par défaut au niveau canal** (ex. `default_ttl = "7d"`), surchargeable **par message** (`ttl` dans le publish).
- L'admin du canal peut définir un **`max_ttl` forcé** : tout message le dépassant est clampé (le publisher reçoit l'info dans la réponse). `ttl = "0"` interdit sauf policy explicite (rétention infinie).
- Purge : job périodique côté serveur (soft-delete → hard-delete) ; les révisions d'une notification expirent avec elle, **et ses pièces jointes aussi** — les objets que plus aucune révision vivante ne référence sont supprimés du bucket (§6.1) **avant** les lignes qui les nomment, parce que ces lignes sont le seul endroit qui sait quel objet appartient à quoi. Un objet encore référencé par une notification vivante n'est jamais touché : rien n'interdit à un publisher de nommer la même pièce jointe dans deux notifications, et supprimer l'image d'une notification vivante serait une perte de données causée par un job de ménage. Les uploads presignés qui n'ont **jamais** été référencés sont couverts par un second balayage : chaque signature est enregistrée (`attachment_presigns`), et une entrée que plus aucune révision vivante ne référence **après un délai de grâce d'un jour** est collectée — le délai est la fenêtre entre « une URL a été signée » et « une notification la nomme », pendant laquelle un identifiant est légitimement non référencé. Un message expiré non livré n'apparaît **pas** dans le backlog de sync — le client reçoit à la place un compteur « N notifications expirées pendant votre absence » par canal.
- La compaction s'applique avant le TTL : au sync, une notification à 40 révisions = 1 entrée.

### 7.3 Accusés (opt-in par canal)

- `delivered` (automatique, par device) et `seen` (interaction utilisateur).
- Si activé sur le canal : les membres autorisés voient « vu par 3/5 » — le numérateur est en **personnes** (deux devices d'une même personne comptent une fois, un service account jamais, §5.2bis), le dénominateur est le nombre de membres humains du canal. **Le dénominateur peut ne pas exister** : un canal dont l'accès vient d'un binding de groupe OIDC n'a pas de liste de lecteurs à compter (§14.2 — l'accès par groupe est dérivé par requête, jamais matérialisé), et l'affichage tombe alors sur « vu par 3 » plutôt que d'inventer un total. Le détail — qui, et quand — se déplie en `HoverCard` (§10.3), servi par `GET /notifications/{id}/receipts` (§8).
- **Ce que l'opt-in gouverne, et ce qu'il ne gouverne pas** (précisé en M4quinvicies, D27) : `receipts_enabled` décide **qui est mis au courant**, jamais si la lecture est enregistrée. Un `seen` est écrit sur **tous** les canaux ; sur un canal qui ne collecte pas, il est diffusé aux seules connexions du lecteur (`Audience::User`) et jamais au canal. La distinction n'est pas cosmétique : `receipts_enabled` vaut **`false` par défaut**, donc lire l'opt-in comme « ne rien écrire » laissait le badge non-lus de §9.2/§10.4 compter éternellement chaque notification d'un canal ordinaire — un nombre qui ne fait que monter. `delivered` garde l'ancienne porte : il est par device, ne nourrit que le panneau de lecture, et aucun badge n'en dépend.

- **Pas de signature des receipts par le device** (concept retiré — les tests d'implémentation ont montré que ce n'était pas viable) : l'authenticité repose sur l'authentification du device auprès du serveur — un principal ne peut poser un receipt que pour ses propres devices, le serveur (de confiance, §6) horodate. Falsifier un receipt suppose de compromettre la session du device, hors du modèle de menace.

### 7.4 Ne pas déranger (DND, par user)

Préférences stockées **côté serveur**, appliquées à tous les devices du user et synchronisées entre clients (`GET/PUT /api/v1/preferences/dnd`, §8) :

- **Quiet hours récurrentes** : plages hebdomadaires avec timezone explicite (`22:00–08:00, lun–ven, Europe/Paris`) — la timezone est celle de la préférence, pas du device : un user en déplacement garde ses nuits.
- **DND ponctuel** : `until` (jusqu'à une heure donnée) ou `for` (durée) — le toggle rapide.
- **Portée** : globale ou par canal, avec **exceptions par canal** (« `cluster-ovh-1` sonne toujours ») — le cas astreinte : tout couper *sauf* le canal critique.
- **Bypass `urgent` opt-in par l'user** (défaut : non) : c'est le destinataire qui décide ce qui peut le réveiller, jamais le publisher (cohérent avec §2.3).

Comportement pendant une période DND :

- Les **push OS sont supprimés** (Web Push, UnifiedPush, APNs) ; le SSE continue de couler (app ouverte = choix conscient de regarder) ; l'inbox se remplit normalement ; les sinks de canal ne sont pas concernés (niveau canal, pas user).
- À la sortie : **un digest, pas une rafale** — un seul push « 12 notifications pendant votre DND (2 failure) », même esprit que le compteur d'expirés du sync (§7.2bis).
- Les notifications différées portent `deferred_by_dnd` dans le sync : le client ne leur applique pas le badge « reçue en retard » (§7.2) — ce retard-là est volontaire.
- **Les clients connectés respectent le DND localement** : desktop Tauri, TUI `--notify-send` et toasts web génèrent leurs notifications locales depuis le SSE — pendant un DND (ou un `silence` actif), ils suppriment ces popups tout en continuant d'alimenter l'inbox. Sans cette règle, le SSE contournerait le DND sur les clients ouverts.
- **Propagation temps réel** : tout changement DND/silence est diffusé aux autres clients du user via l'événement `preference` (§8.1) — activer le DND sur un client le fait respecter partout, immédiatement.

Articulation avec `silence` (§2.3) : `silence` est **réactif et ciblé** (une notification, un `group_key`), le DND est **planifié et large** (du temps, des canaux). Les deux sont par user et côté serveur.

---

## 8. API (Actix + Utoipa)

Endpoints principaux (OpenAPI complète générée par Utoipa, servie sur `/api/docs`) :

```
GET    /api/v1/channels                       # canaux visibles par le principal (paginé)
POST   /api/v1/channels                       # créer un canal (dynamic)
GET    /api/v1/channels/{id}                  # détail : membres, TTL, sinks, badge managed
PATCH  /api/v1/channels/{id}                  # policy : TTL, priorité par défaut, receipts — refusé si managed
DELETE /api/v1/channels/{id}                  # refusé si managed (GitOps source de vérité)
POST   /api/v1/channels/{id}/members          # inviter
DELETE /api/v1/channels/{id}/members/{principalId} # retirer un membre (user ou service account) / quitter
GET    /api/v1/channels/{id}/notifications    # inbox paginée (filtres state/status/tag/priority)
PUT    /api/v1/channels/{id}/notifications/{external_id}   # upsert idempotent
POST   /api/v1/notifications                  # publish simple (canal cible dans le body)
GET    /api/v1/notifications/{id}             # détail + historique des révisions
DELETE /api/v1/notifications/{id}             # suppression (tombstone diffusé, §8.1)
POST   /api/v1/notifications/{id}/interactions  # ack / reply / choice (§2.3)
GET    /api/v1/notifications/{id}/interactions  # queryable par les services intéressés
POST   /api/v1/notifications/{id}/silence       # snooze par user (scope notification|group, §2.3)
GET    /api/v1/search?q=...                   # recherche notifications (titre/body/tags) + canaux, scopée aux canaux du principal — palette web (§10.4)
GET    /api/v1/unread                         # non-lus par canal — le badge de §7.3/§9.2/§10.4/§10bis ; un canal sans non-lu est absent, jamais à zéro
GET    /api/v1/sync?cursor=...                # backlog offline
GET    /api/v1/stream                         # SSE (contrat §8.1)
GET    /api/v1/ws                             # WebSocket (mêmes événements, frames JSON)
POST   /api/v1/receipts                       # delivered/seen
GET    /api/v1/notifications/{id}/receipts    # qui l'a lue et quand — ce que déplie « vu par 3/5 » (§7.3, §10.3) ; `null` si le canal n'en collecte pas
GET    /api/v1/push/webpush                   # clé publique VAPID (`applicationServerKey`, RFC 8292) — 501 si ce déploiement ne fait pas de Web Push
GET    /api/v1/preferences/dnd                # préférences DND du user (§7.4)
PUT    /api/v1/preferences/dnd                # quiet hours, exceptions par canal, bypass urgent
GET    /api/v1/devices                        # devices du principal
POST   /api/v1/devices                        # enrôlement device (transport push ; pk X25519 requise pour APNs/UnifiedPush)
DELETE /api/v1/devices/{id}                   # révocation : push subs + curseur (+ clé publique si mobile) (§15)
GET    /api/v1/users/me/profile               # profil : display_name, avatar, timezone, locale (§2.4)
PUT    /api/v1/users/me/profile               # override des champs synchronisés OIDC
PUT    /api/v1/users/me/avatar                # upload direct ≤ 512 KiB, resize serveur 256×256 (§2.4)
GET    /api/v1/serviceaccounts                # liste + profils (§5.2bis)
POST   /api/v1/serviceaccounts                # créer (dynamic) — les managed viennent du GitOps §11
DELETE /api/v1/serviceaccounts/{id}           # désactivation : révoque ses clés, coupe ses SSE
GET    /api/v1/apikeys                        # métadonnées (préfixe, scopes, last_used) — jamais le secret
POST   /api/v1/apikeys                        # création — secret retourné une seule fois
DELETE /api/v1/apikeys/{id}                   # révocation immédiate
PATCH  /api/v1/apikeys/{id}/limit             # plafond de publication propre à la clé (§8.2, D24) — platform-admin seulement, jamais son porteur
POST   /api/v1/auth/token/exchange            # fédération JWT CI
POST   /api/v1/attachments/presign             # upload direct au bucket : le corps nomme le canal (autorisation `publish`), TTL court, taille/type imposés §6.1
GET    /api/v1/attachments/{id}                # download signé : autorisé par une notification lisible qui l'affiche (§6.1) — jamais les octets, 409 (`notification.gone`) si invisible
GET    /api/v1/attachments/{id}/thumbnail      # la vignette dérivée d'une row (§10.3, D30) — octets et non signée, même 409 si invisible ou rien dérivé
GET    /api/v1/admin/audit                   # audit log browsable (filtres action/canal/période) — rôle §5.4 uniquement
GET    /api/v1/admin/channels                # inventaire : fiches de métadonnées, jamais de contenu (§5.4, §10.4bis) — rôle §5.4
POST   /api/v1/admin/channels/{id}/join      # self-join audité : le seul chemin du rôle vers le contenu (§5.4, T30)
GET    /api/v1/admin/trust-policies          # policies configurées, lecture seule (§5.3, §11) — rôle §5.4
GET    /api/v1/admin/dlq                      # sinks en souffrance par canal (§7.1bis) — rôle §5.4
POST   /api/v1/admin/dlq/{id}/requeue         # requeue manuel d'une livraison sink (§7.1bis) — rôle §5.4
GET    /api/v1/admin/reconciliation           # état GitOps : drift corrigé, dernier SIGHUP, managed vs dynamic (§11) — rôle §5.4
```

Conventions transverses :
- **Pagination cursor-based** partout (`?cursor=...&limit=...`, curseur opaque) — même format que le curseur de sync.
- **Erreurs** : `application/problem+json` (RFC 9457) avec un champ `code` stable, consommé tel quel par CLI/TUI/web pour les messages utilisateur.
- **Idempotence** : le `PUT .../{external_id}` l'est par construction ; `POST /notifications` accepte un header `Idempotency-Key` (la même réponse est rejouée pendant 24 h).

### 8.1 Flux SSE (contrat)

`GET /api/v1/stream` (`text/event-stream`), filtrable par `?channels=a,b`. Chaque événement porte :

```
id: <cursor>          # curseur opaque global — le même que GET /sync ; Last-Event-ID le rejoue
event: revision | receipt | interaction | tombstone | preference
data: <JSON>
```

Le `data:` est l'**événement entier** — enveloppe comprise, pas seulement le payload. C'est ce qui rend SSE et WebSocket rigoureusement identiques (le WS n'a pas de ligne `id:`, il lui faut donc le curseur dans le JSON), et surtout ce qui permet à un client qui n'a jamais vu une notification de la classer sans un `GET /notifications/{id}` par arrivée — exactement le suivi que la dénormalisation de `revision` sert à éviter.

L'objet est **plat** : l'enveloppe et le payload partagent un seul espace de noms. Deux noms sont donc réservés par l'enveloppe et par elle seule — `channel_id`/`user_id` (jamais `id`, qu'un `receipt` et une `interaction` portent déjà) et `logged_at` (jamais `recorded_at`, qu'une `interaction` porte déjà). Un champ de payload qui viendrait réclamer l'un des deux casse le fil, pas un détail : sérialisé deux fois, l'objet ne se relit plus.

```jsonc
// event: revision — la création est incluse (créer = pousser la révision seq=1)
{
  "cursor": "evt_00000000000042",   // le même que la ligne `id:` ; le WS n'a que celui-ci
  "audience": "channel",
  "channel_id": "ci-infra",
  "logged_at": "2026-08-01T10:12:41Z",   // quand la ligne a été journalisée
  "event": "revision",
  "notification_id": "ntf_01J...",
  "external_id": "run-4521",
  "seq": 3,
  "state": "closed", "status": "success", "priority": "default",
  "author": { "kind": "service_account", "id": "bot-argocd", "display_name": "ArgoCD" },  // §2.4/§5
  "created_at": "2026-08-01T10:00:00Z", "revised_at": "2026-08-01T10:12:41Z",
  "payload": { /* §2.3 — toujours en clair : SSE/WS est du TLS direct, la sealed box
                  n'existe que dans les blobs push mobiles (§6.2) */ }
}
```

- `tombstone` : `{ "channel_id", "notification_id", "reason": "deleted" | "expired" }` — le client retire l'entrée localement.
- `receipt` / `interaction` : diffusés aux membres autorisés du canal (si receipts actifs, §7.3).
- `preference` : émis **uniquement aux connexions du user concerné** — état DND et silences actifs (§7.4) ; ses autres clients se mettent à jour en temps réel.
- Keep-alive : commentaire SSE (`: ping`) toutes les 25 s, sans `id` (proxy-friendly).
- **Un seul mécanisme de reprise** : `Last-Event-ID` (SSE) et `GET /sync?cursor=` consomment le même curseur ; le WebSocket transporte les mêmes événements en frames JSON.

### 8.2 Limites & quotas

Défauts globaux, surchargeables par canal (`limits` dans `[[channels.managed]]`, §11) et par API key. Violation → `429` (`Retry-After`), `413` ou `422` selon le cas :

| Limite | Défaut | Note |
|---|---|---|
| Taille payload (hors attachments) | 64 KiB | data-uri image comprise (≤ 48 KiB, §2.3 — au-delà, passer par un attachment) |
| Taille attachment | 25 MiB | presign refusé au-delà |
| Publish par API key | 60/min, burst 120 | tous canaux confondus |
| Publish par canal | 600/min | protège le fan-out |
| Révisions par notification | 1 000 | au-delà : `422` — ouvrir une nouvelle notification |
| Token exchange | 10/min par (issuer, subject) | anti-abus fédération |
| Connexions SSE/WS par user | 20 | multi-onglets + devices |
| Actions par notification | 5 | ordre = priorité d'affichage (§2.3) |
| Avatar | 512 KiB | resize serveur 256×256, formats statiques only (§2.4) |
| Recherche (`/search`) | 30/min par principal | plein texte côté serveur — la palette (§10.4) debounce côté client |

### 8.3 Chemins de déclenchement (vue d'ensemble)

Tous les moyens de déclencher une notification convergent vers **deux endpoints d'ingestion** ; ce qui varie, c'est le producteur et l'identité qu'il présente (§5). La CLI (`notify publish`/`update`, §9.1), le mode `:` du TUI (§9.2) et `curl` ne sont que des enrobages de ces mêmes appels — il n'existe aucun protocole d'ingestion parallèle. Les sinks (§7.1bis) sont sortants uniquement et les interactions (§2.3) ne créent pas de notification.

```mermaid
flowchart LR
  subgraph producteurs [Producteurs]
    human["Humain<br/>(web / CLI / TUI)"]
    ci["Job CI<br/>(Forgejo / GitHub Actions)"]
    bot["Bot durable<br/>(ArgoCD, bridge...)"]
    script["Script perso<br/>(cron, dotfiles, curl)"]
    legacy["Outil de canal<br/>(pipeline partagé)"]
  end

  subgraph authn ["AuthN (§5)"]
    oidc["Token OIDC<br/>(§5.1)"]
    exchange["POST /auth/token/exchange<br/>jeton Ed25519 ≤ 15 min (§5.3)"]
    sakey["API key service account<br/>(§5.2bis)"]
    userkey["API key utilisateur<br/>(§5.2)"]
    chankey["API key canal<br/>publish-only, anonyme (§5.2)"]
  end

  subgraph ingestion ["Ingestion (§8)"]
    put["PUT /channels/{id}/notifications/{external_id}<br/>upsert idempotent"]
    post["POST /notifications<br/>Idempotency-Key optionnel"]
  end

  state{"champ state<br/>fourni ? (§2.2)"}

  lifecycle["Notification à cycle de vie<br/>open ⇄ working → closed(status)<br/>révisions LWW, TTL à la fermeture"]
  simple["Notification simple (fire-and-forget)<br/>créée closed(status ?? info)<br/>TTL dès la création"]

  human --> oidc
  ci --> exchange
  bot --> sakey
  script --> userkey
  legacy --> chankey

  oidc --> put & post
  exchange --> put & post
  sakey --> put & post
  userkey --> put & post
  chankey --> put & post

  put --> state
  post --> state
  state -- "oui" --> lifecycle
  state -- "non" --> simple
```

### Exemples curl

```bash
# CI : début de pipeline (API key canal)
curl -X PUT "$NOTIFY_URL/api/v1/channels/ci-infra/notifications/run-$CI_RUN_ID" \
  -H "Authorization: Bearer $WNK_CHANNEL_KEY" \
  -H "Content-Type: application/json" \
  -d '{ "title": "Pipeline infra #4521", "state": "open", "status": "info", "ttl": "48h",
        "actions": [{ "type": "view", "label": "Logs", "url": "https://forgejo.weebo.poc/..." }] }'

# Fin de pipeline : même id, révision finale
curl -X PUT "$NOTIFY_URL/api/v1/channels/ci-infra/notifications/run-$CI_RUN_ID" \
  -H "Authorization: Bearer $WNK_CHANNEL_KEY" \
  -H "Content-Type: application/json" \
  -d '{ "state": "closed", "status": "success" }'
```

> Le JSON transite en clair sur TLS — curl suffit, aucune crypto côté publisher ; au repos, la protection est infra (disques/backups chiffrés, §6.1).

---

## 9. CLI & TUI

Binaire unique `notify`, deux modes dans le même exécutable : commandes one-shot scriptables (**clap**) et client temps réel plein écran (**ratatui**). **Le TUI est le mode par défaut** : `notify` sans sous-commande ouvre l'interface (`notify watch` reste un alias explicite, utile en doc/scripts) ; toute sous-commande clap court-circuite le TUI pour le mode one-shot.

**Règle de parité CLI ⟷ TUI** : tout ce qui est faisable en CLI l'est aussi depuis le TUI. Cette parité n'est pas maintenue à la main : le TUI embarque un **mode commande** (`:`) qui parse la ligne saisie avec **le même parseur clap** que le shell (§9.2) — une nouvelle sous-commande est donc disponible dans le TUI par construction, sans code supplémentaire.

### 9.1 CLI — clap

**clap v4 en mode derive** : l'arbre de commandes est un enum `Commands` typé, ce qui donne gratuitement `--help` structuré, complétions shell (`clap_complete` : bash/zsh/fish, générées par `notify completions <shell>` et packagées par la CI release §3.2), et erreurs d'usage propres.

```
notify                            # sans sous-commande : ouvre le TUI (§9.2)
notify publish  -c cluster-ovh-1 -t "etcd degraded" --status failure --image ./graph.png \
                --action "view:Runbook:https://..."
notify update   -c ci-infra --id run-42 --state closed --status success
notify watch                      # alias explicite du TUI (scripts, doc)
notify channel  create|join|set-ttl
notify device   enrol             # (alias : `enroll`) enregistre le device (curseur de sync) — zéro crypto : la CLI reçoit via SSE/TLS (§6.2)
notify token    exchange          # depuis un job CI
notify dnd      on|off|until 9h|for 2h|schedule|status   # DND serveur (§7.4) — schedule édite les quiet hours ; dispo dans le TUI via :dnd
notify completions <shell>        # complétions clap_complete
```

> Garde-fou non-interactif : si stdout n'est pas un TTY (pipe, cron, CI), `notify` sans sous-commande n'ouvre pas le TUI — il affiche l'aide et sort avec le code 2, pour ne jamais bloquer un script.

Conventions :
- **Config en couches** (même philosophie que le serveur §11) : défauts < `~/.config/weebo-notify/config.toml` < env `WNK_*` < flags clap. Un flag global `--profile` permet plusieurs serveurs (perso, boulot).
- **Sortie scriptable** : chaque commande accepte `--output json|table` ; les erreurs partent sur stderr avec exit codes stables (0 ok, 1 erreur API, 2 usage) — utilisable en CI sans parsing fragile.
- Les valeurs contraintes (`--status`, `--state`, `--priority`) sont des enums clap (`ValueEnum`) dérivées des types de `notify-domain` → CLI et serveur ne peuvent pas diverger.
- `publish`/`update` lisent aussi le body sur stdin, **quand on le demande** : `cmd | notify publish -c ... --body -`. Le `-` est explicite et pas décoratif — une CLI qui lit stdin sans qu'on lui dise est une CLI qui se bloque un jour dans un script, et le drapeau coûte cinq caractères. (Écrit ici après avoir diffé §9 contre l'arbre clap : la version précédente de cette ligne promettait « `-` ou pipe » et donnait un exemple sans `-`.)
- **Tokens** : via le coffre OS quand il existe (§10bis.2) ; sur hôte headless sans Secret Service (serveur, conteneur), fallback fichier `0600` dans le répertoire du profil — et l'usage éphémère (pod de debug, API key en env) fonctionne sans aucune persistance (§9.3bis).

### 9.2 TUI — ratatui

`notify` (sans sous-commande, ou `notify watch`) ouvre l'interface plein écran, construite sur **ratatui + crossterm** (backend par défaut, portable Linux/macOS/Windows) :

- **Architecture Elm-like** (le pattern recommandé ratatui) : un `App` état pur, un enum `Message` (touche, tick, événement SSE, réponse API), une fonction `update(app, msg)` et un `draw(frame, app)` sans effet de bord. Les I/O vivent dans des tâches tokio qui poussent des `Message` dans un `mpsc` — le rendu ne bloque jamais sur le réseau.
- **Flux temps réel** : la connexion SSE (`GET /api/v1/stream`, reconnect via `Last-Event-ID` §7.1) tourne dans sa tâche ; chaque événement devient un `Message::Notification` qui met à jour la liste sans redraw global (draw uniquement sur message ou tick, pas de boucle busy). Sur une machine au store partagé (§9.3bis), cette tâche est soit **détentrice du SSE**, soit **lectrice du store** réveillée par le signal local, selon l'élection — transparent pour l'`App` : dans les deux cas il en sort des `Message`.
- **Layout 3 zones** — miroir volontaire du shell web (§10.4), mêmes réflexes d'un client à l'autre :

```
┌─ Channels ──┬─ Inbox ────────────────────────┬─ Detail ─────────────┐
│ cluster-ovh │ ▶ etcd degraded      ✗ failure │ Timeline révisions   │
│ ci-infra  3 │   Pipeline #4521     ⟳ working │ open → working → ... │
│ dev-team    │   MR !42 ouverte     ● info    │ Actions: [A]ck [V]iew│
│             │   (+2h13 en retard)            │ Receipts: vu 3/5     │
└─────────────┴────────────────────────────────┴──────────────────────┘
 j/k naviguer · tab panneau · a ack · v view · / filtre · : commande · ? aide · q quitter
```

- Widgets : `List` stateful (inbox, virtualisée par la pagination du sync), `Table` (receipts), `Gauge` (champ `progress` §2.3), `Paragraph` markdown dégradé pour le body, `Tabs` (filtres par état), popup centré pour l'aide et la saisie `reply`.
- **Clavier only** : bindings vim-like (`j/k`, `g/G`, `/` filtre fuzzy, `a` ack, `r` reply, `s` silence (popup durée/scope), `c` choice (popup options), `v` ouvre l'URL via `open`/`xdg-open`, `Enter` détail, `Esc` retour). La barre du bas affiche en permanence les bindings actifs — découvrabilité identique à la command palette web.
- **Mode commande `:`** (parité CLI, §9) : `:` ouvre une ligne de commande en bas d'écran qui accepte **exactement la grammaire clap** — la saisie est découpée (shell-words) puis passée à `Command::try_parse_from`, et le résultat s'affiche dans un popup (sortie `--output table` rendue en widget, erreurs clap comprises). Exemples : `:publish -c ci-infra -t "rebuild ok" --status success`, `:channel set-ttl ci-infra 24h`, `:device enroll`, `:token exchange`. Complétion contextuelle depuis l'arbre clap + historique persisté. Le contexte courant pré-remplit les arguments (`:update --state closed --status success` sur la notification sélectionnée n'exige pas `--id`).
- Les opérations fréquentes gardent en plus une **affordance native** (binding direct ou formulaire popup) : `a`/`r` sur une notification, assistant `device enroll` au premier lancement, création de canal via formulaire — le mode `:` est le filet de sécurité qui garantit que *tout* le reste est accessible sans quitter le TUI.
- Indicateurs partagés avec le web : pastille+icône de statut (jamais couleur seule), badge retard `(+2h13)`, spinner sur `open`/`working`, compteur non-lus par canal dans la sidebar.
- **Passthrough notifications système** : en plus du rendu TUI, option `--notify-send` qui relaie vers les notifications desktop (notify-rust). *Amendé en M4quaterdecies* : la clause « quand le terminal n'a pas le focus » est **retirée**, parce qu'il n'existe aucune façon portable de le savoir — X11, Wayland, macOS et Windows répondent différemment ou pas du tout, et un multiplexeur casse la réponse qu'on choisit. Le drapeau relaie quand il est allumé ; c'est à la personne de l'éteindre. Le DND de §7.4 s'applique dans les deux cas.
- Résilience terminal : hook panic qui restaure le terminal (raw mode/alternate screen) avant de crasher — un bug ne doit jamais laisser le shell cassé.
- Sécurité d'affichage (§2.3) : tous les champs texte sont **strippés des séquences ANSI/contrôle** avant rendu (défense en profondeur, le serveur strippe déjà à l'ingestion) ; `v`/`view` n'ouvre que des URLs `http(s)`.

### 9.3 Partage CLI/TUI

Les deux modes consomment **`notify-client-api`** (client généré) et **`notify-client-core`** (§4) : cache local, curseur de sync, reconnexion SSE. La crypto device (X25519, sealed box) n'existe que dans les builds Tauri mobiles de `notify-client-core` (§6.2) — la CLI n'en embarque pas. La CLI et le TUI ne contiennent que de la présentation — toute logique métier qui apparaîtrait dans `notify-cli` doit redescendre dans `notify-client-core` (elle servira aussi à Tauri).

### 9.3bis Un seul état local par machine

Quand la CLI/TUI cohabite avec l'app desktop (§10bis), elles ne vivent pas chacune dans leur coin — même inbox, même curseur, même lu/non-lu :

- **Un store local unique par profil** : `notify-client-core` persiste cache et curseur de sync dans un **SQLite en mode WAL** à un chemin standard (`$XDG_DATA_HOME/weebo-notify/<profile>/store.db`, équivalents macOS/Windows). Le scope est le **profil** (§9.1 `--profile`, un par serveur), pas la machine : toutes les frontales pointant sur le même profil ouvrent le même fichier — le WAL rend l'accès multi-process sûr ; deux profils = deux stores, deux devices, deux locks.
- **Une seule identité device** : l'enrôlement est **par profil, pas par frontale** — CLI, TUI et app desktop partagent le même device (un seul curseur côté serveur) et les mêmes tokens via le coffre OS (§10bis.2). Corollaire : les receipts « vu par 3/5 » (§7.3) comptent la machine une fois, pas trois.
- **Un seul flux temps réel** : une élection triviale par **fichier de lock** à côté du store désigne la frontale qui tient le SSE et alimente le store ; les autres lisent le store et sont réveillées par un signal local (socket unix / named pipe). L'app desktop en tray gagne naturellement l'élection (toujours là) — `notify ls` répond alors depuis le cache chaud sans toucher au réseau ; si rien ne tourne, la première frontale démarrée prend le flux, et le lock se libère proprement à sa mort (lock advisory, pas un flag en base).
- **Écritures toujours via le serveur** : un `ack` fait dans le TUI part au serveur, revient par le SSE et se matérialise dans le store pour toutes les frontales — aucune écriture d'état métier croisée entre clients locaux, le serveur reste l'unique source de vérité. Le store n'est qu'un cache : le supprimer déclenche un re-sync complet, jamais une perte.

Un `notify watch` éphémère **hors machine enrôlée** (pod de debug, §7.1) reste possible : sans store ni device, il streame en direct — le partage d'état est un confort local, pas un prérequis.

---

## 10. Frontend

- **Vue 3 + Vite + Tailwind + PWA** (vite-plugin-pwa) : service worker → Web Push, cache offline du backlog, badge de retard.
- **Tauri v2** : même frontend Vue empaqueté desktop + Android + iOS — section dédiée §10bis (architecture webview/cœur Rust, auth, push mobile, packaging).
- Vues : inbox groupée (`group_key`), timeline d'une notification (révisions), gestion canaux/membres/clés, administration trust policies et API keys.

### 10.1 Design system — shadcn-vue

Direction : une UI **moderne, dense et sobre**, type outil d'infra (Linear/Vercel-like), pas un dashboard générique. La stack UI est **[shadcn-vue](https://www.shadcn-vue.com/)** — les composants sont **copiés dans le repo** (pas une dépendance npm de composants), construits sur les primitives headless **Reka UI** :

| Couche | Choix | Rôle |
|---|---|---|
| Primitives | **Reka UI** (ex Radix Vue) | Accessibilité, focus management, comportements (Dialog, Popover, Combobox...) |
| Composants | **shadcn-vue** (vendorés dans `web/src/components/ui/`) | Ownership total du code, personnalisation sans fork ni surcharge CSS |
| Styling | **Tailwind CSS v4** | Tokens via variables CSS natives (`@theme`), zéro config JS |
| Icônes | **lucide-vue-next** | Set cohérent avec l'esthétique shadcn |
| Charts | (plus tard) composants chart shadcn-vue | Stats canal / delivery — pas avant M3 |

Pourquoi shadcn-vue plutôt qu'une lib de composants (PrimeVue, Vuetify, Naive) :
- Les composants vivent dans le repo → le design system `monofolio` s'intègre en **éditant les composants**, pas en le contorsionnant par-dessus une lib opinionated.
- Theming 100 % variables CSS → un thème = un fichier de tokens ; `monofolio` deviendra un simple preset de tokens + ajustements ciblés.
- Reka UI porte l'accessibilité (clavier, ARIA, focus trap) — critique pour une inbox pilotable au clavier (parité avec le TUI §9).

### 10.2 Tokens & thèmes

- Tokens sémantiques shadcn (`--background`, `--foreground`, `--primary`, `--muted`, `--destructive`, `--border`, `--radius`...) définis en **OKLCH** dans un seul fichier `web/src/styles/theme.css`.
- **Dark mode par défaut** (public sysadmin/dev), light mode complet, bascule `class` sur `<html>` + suivi `prefers-color-scheme` ; le choix persiste (localStorage + sync par device plus tard).
- Tokens **métier** au-dessus des tokens shadcn, consommés par les composants notification :
  - `--status-success | failure | warning | info | cancelled` (état de fermeture),
  - `--priority-min → urgent` (échelle discrète, pas un dégradé),
  - `--state-open | working | closed`.
  Règle : les composants ne référencent **jamais** une couleur brute, uniquement ces tokens — le swap `monofolio` reste alors indolore.
- Densité : `compact` par défaut sur desktop (inbox = outil de travail), `comfortable` sur mobile/Tauri via le même code (breakpoints + token `--density`).

### 10.3 Langage visuel des notifications

La notification est l'objet central ; son rendu est un composant unique décliné (row d'inbox, card détail, toast temps réel, notification OS) :

- **Statut** = pastille colorée + icône (jamais la couleur seule — daltonisme), **priorité** = accent discret sur le bord gauche de la row (urgent = accent + badge, pas de clignotement).
- **Cycle de vie visible** : `open`/`working` affichent un indicateur vivant (spinner discret, `progress` si fourni) ; `closed` fige la row avec son statut final. La **timeline des révisions** (§2.2) se déroule dans le panneau détail — chaque révision est un événement horodaté, diff mis en évidence (ex. `status: info → failure`).
- **Badge retard** (§7.2) : chip `reçue en retard (+2h13)` sur la row, tooltip avec les deux timestamps.
- **Rendu markdown sanitizé** (§2.3) : markdown pur, HTML brut jamais interprété, liens `http(s)` uniquement, CSP stricte sans `unsafe-inline` — le contenu d'un canal peut venir de publishers moins fiables que ses lecteurs. La politique **est rendue par nginx au démarrage du conteneur** (ADR-0021, qui a remplacé la forme « voyage avec le bundle » d'ADR-0019 le jour où deux pipelines de publication ont rendu un déploiement par build intenable) : `script-src` sans `unsafe-inline` avec le hash du seul script inline de la page — figé au build, seule partie qui l'est encore —, `connect-src` limité à l'API et au fournisseur ; `style-src` garde `'unsafe-inline'`, seul écart, et l'ADR dit pourquoi. `img-src` nomme trois sources et pas une de plus — `'self'`, `data:` pour les images inline de §2.3, et **l'origine du stockage objet** (`WNK_ATTACHMENT_ORIGIN`, variable d'environnement du conteneur plutôt que variable de build), sans laquelle une pièce jointe présignée est bloquée par la page elle-même (§6.1, D26) ; elle est optionnelle, car un déploiement sans objets est un déploiement qui marche.
- **Image de notification** (§2.3) : les deux genres sont rendus dans le panneau détail — un `data:` directement, un attachment via l'URL signée de `GET /attachments/{id}`. Une image qu'on ne peut pas obtenir se dit (« l'image jointe n'a pas pu être affichée ») plutôt que de disparaître : un publisher qui voit un trou conclut que son `--image` a été ignoré.
- **Actions** (§2.3) : boutons shadcn `Button` en pied de card — `view` = variante `link`/`outline`, `ack` = `default` (état acquitté persistant), `reply` = `ghost` + input inline, `choice` = groupe de boutons `outline` (l'option retenue passe en `default`), `silence` = `ghost` + menu durée/scope (`DropdownMenu`). Receipts « vu par 3/5 » — le compte est en **users** (§7.3, `seen` est une interaction utilisateur) — en `HoverCard` listant qui a vu et quand ; le `delivered` par device reste un détail replié. Le dénominateur disparaît (« vu par 3 ») là où le canal n'a pas d'audience énumérable, la carte disant pourquoi (§7.3, §14.2).
- Composants shadcn mobilisés : `Card`, `Badge`, `Button`, `DropdownMenu` (actions row), `Dialog`/`Sheet` (détail mobile), `Tabs`, `Tooltip`, `HoverCard`, `Command`, `Table` (admin), `Form` + `vee-validate`/`zod` (admin), `Sonner` (toasts temps réel), `Skeleton` (chargement backlog).

### 10.4 Layout & écrans

Shell applicatif **3 zones**, responsive (le même code sert PWA, desktop Tauri et mobile) :

```
┌──────────┬───────────────────────────┬──────────────────┐
│ Sidebar  │ Inbox (liste virtuelle)   │ Panneau détail   │
│ canaux + │ groupée par group_key,    │ timeline des     │
│ filtres  │ filtres tag/priorité/état │ révisions,       │
│ sauvés   │ + barre de recherche      │ actions, receipts│
└──────────┴───────────────────────────┴──────────────────┘
```

- **Command palette / quick switcher** (`Cmd+K` — `Ctrl+K` hors macOS ; composant `Command` dans un `Dialog` modal) : **une seule surface pour chercher et switcher** partout dans l'app, identique en web et en Tauri (§10bis.3quater) :
  - **Switcher** : fuzzy sur les canaux, les filtres sauvés et les écrans (`Canaux`, `Devices`, `Admin`…) — `Enter` navigue, l'inbox conserve son état au retour.
  - **Recherche** : notifications (titre, body, tags) — via l'endpoint de recherche de l'API en web, via le cache local `notify-client-core` en Tauri (instantané, offline). Périmètre assumé : le cache local est borné au backlog synchronisé — la dernière ligne de résultats en Tauri est « chercher sur le serveur » (bascule sur l'API), pour que « trouvable en web mais pas dans l'app » ne soit jamais perçu comme un bug. Résultats groupés (Canaux / Notifications / Actions), `Enter` ouvre le panneau détail.
  - **Actions contextuelles** : ack, mark seen, « DND 1h » (§7.4), création de canal (admin) — mêmes verbes que le mode `:` du TUI (§9.2), vocabulaire commun entre clients.
  - Modal au-dessus de tout, `Esc` ferme et rend le focus à l'élément d'origine — l'app doit être pilotable sans souris.
- **Permaliens** : `/n/<id>` (notification, panneau détail) et `/c/<id>` (canal) — miroir exact des deep links `weebo-notify://` (§10bis.3quater) : mails de sinks (§7.1bis), logs CI et notifications OS pointent l'un ou l'autre selon la cible.
- Mobile/Tauri : sidebar → `Sheet`, panneau détail → navigation pleine page ; safe-areas iOS gérées.
- Écrans : **Inbox** (défaut), **Notification** (timeline, avec `author` — avatar + display_name §2.4), **Canaux** (liste + badge `managed` §11, membres, TTL, sinks), **Devices** (enrôlement, révocation), **Admin** (trust policies, API keys — création avec reveal unique du secret —, service accounts §5.2bis ; s'élargit en **console plateforme** §10.4bis pour le rôle §5.4), **Settings** (profil §2.4 — avatar, display_name, timezone —, thème, densité, langue, DND §7.4 — quiet hours et exceptions par canal), **Apps** (téléchargements desktop / CLI / APK pointés sur les releases §3.2bis, canal updater `stable`/`beta`).
- États vides et erreurs soignés dès M1 (inbox vide ≠ erreur de sync ≠ backlog expiré « N notifications expirées » §7.2bis).

### 10.4bis Console plateforme (rôle §5.4)

Visible uniquement au `platform-admin` — une **console d'exploitation**, pas un dashboard : des états et des compteurs **actionnables** ; les séries temporelles restent à Grafana (§13), la dupliquer serait le « dashboard générique » que §10.1 refuse.

- **V1 (M4)** — les objets d'exploitation qui n'ont aucune autre UI :
  - **DLQ des sinks** (§7.1bis) : livraisons en souffrance par canal, détail de l'échec, requeue unitaire ou par canal ;
  - **Audit log** (§13) : browsable, filtres principal/action/période — l'outil d'après-incident ;
  - **Réconciliation GitOps** (§11) : canaux managed vs dynamic, drift corrigé au dernier SIGHUP/boot, `on_removed` appliqués ;
  - **Inventaire** : tous les canaux (fiches de métadonnées uniquement), users et leurs devices (révocation, §15), service accounts, API keys expirées ou dormantes (`last_used`, §5.2).
- **Plus tard** : usage vs quotas par canal (§8.2), santé des replicas (leader §11, eventbus, jauge de connexions §13).
- **Frontière de contenu** (§5.4) : aucune notification n'est lisible depuis la console — un canal y est une fiche, pas une inbox ; « rejoindre le canal » est le chemin explicite, et audité, pour en voir le contenu.

### 10.5 Motion & temps réel

- Arrivée SSE : insertion **sans layout shift** (la liste ne saute pas sous le curseur) — nouvelle row annoncée par un séparateur « N nouvelles », pattern inbox.
- Mise à jour d'une révision : transition douce du statut (fade du token couleur), jamais de re-render visible de toute la liste (liste virtualisée, key = `notification_id`).
- Animations utilitaires uniquement (150–200 ms, `ease-out`), respect de `prefers-reduced-motion`.

### 10.6 Intégration `monofolio`

`monofolio` reste la cible design long terme. Le contrat : tant qu'il n'est pas prêt, on vit sur le thème shadcn par défaut (neutre, déjà propre) ; son intégration = remplacement du fichier de tokens + retouches ciblées des composants vendorés. Aucune vue ne doit référencer autre chose que les tokens sémantiques de §10.2 — c'est la seule règle bloquante en review front.

---

## 10bis. Tauri (desktop, Android, iOS)

**Tauri v2** empaquette le frontend Vue (§10) en applications natives desktop (Linux, macOS, Windows) et mobiles (Android, iOS). Ce n'est pas un client de plus mais **le même front dans une coquille Rust** — la coquille apporte ce que le navigateur ne peut pas : notifications OS fiables, push mobile, keystore matériel, présence en arrière-plan.

### 10bis.1 Architecture — webview de présentation, cœur Rust

- L'app vit dans **`web/src-tauri/`** (convention Tauri, membre du workspace cargo) et embarque **`notify-client-core`** (§4) : c'est le processus Rust qui possède le réseau (SSE, reconnexion `Last-Event-ID` §7.1), le cache local et le curseur de sync (store partagé avec la CLI/TUI sur desktop, §9.3bis) et la crypto device (builds mobiles uniquement, §6.2). La webview reste de la **présentation pure** — même règle que la CLI/TUI (§9.3) : toute logique métier qui apparaîtrait côté webview redescend dans `notify-client-core`.
- Le front consomme une **façade TS unique** (`web/src/lib/client/`) à deux implémentations résolues au build : **HTTP/SSE direct** (PWA) et **IPC Tauri** (commands + events vers `notify-client-core`). Aucun composant Vue ne sait s'il tourne en PWA ou en Tauri — le shell 3 zones, les tokens et la densité s'adaptent déjà par breakpoints (§10.2, §10.4).
- Événements Rust → webview via les **events Tauri** typés, miroir des événements SSE (§8.1) ; le store front est identique dans les deux modes, seule la source change.

### 10bis.2 Auth — PKCE via navigateur système

- Authorization Code + **PKCE dans le navigateur système**, jamais dans la webview — l'utilisateur doit voir l'URL d'Authentik et profiter de sa session et de ses passkeys. Retour du callback : sur desktop, **loopback RFC 8252** (`http://127.0.0.1:<port éphémère>`) — insensible au scheme-hijacking, un custom scheme pouvant être enregistré par n'importe quelle app — avec `weebo-notify://oauth/callback` en fallback ; sur mobile, **App Links / Universal Links** (vérifiés par l'OS, pas de hijack possible).
- Tokens stockés dans le **coffre de l'OS** (Secret Service / Keychain / Credential Manager sur desktop, Keystore/Keychain sur mobile — plugin keyring) — jamais en localStorage ni dans la webview, cohérent avec §5.1. Le refresh silencieux (rotation §5.1) vit côté Rust dans `notify-client-core`.

### 10bis.3 Desktop (M5)

- **Temps réel** : SSE direct (§7.1), zéro clé device (§6.2). Les notifications OS sont générées localement depuis le flux SSE (plugin notification) et **respectent DND et `silence` localement** (§7.4) — l'inbox continue de se remplir, les popups se taisent.
- **App de fond** : l'app vit dans le **tray** (section dédiée §10bis.3ter), **single instance** (relancer focus la fenêtre existante), **autostart opt-in** (décoché par défaut — démarre alors directement en tray, fenêtre cachée).
- Actions sur la notification OS quand l'OS le permet : `ack` en bouton d'action ; clic = focus de l'app sur le panneau détail de la notification.
- **Updater Tauri signé** (minisign), manifest **self-hosted** servi par la release CI Forgejo (§3.2bis) — aucune dépendance à un service tiers. Deux canaux : `stable` (défaut) et `beta` (pre-releases `rc`, opt-in dans Settings). Le self-update ne s'applique qu'aux formats que l'updater sait remplacer (AppImage, `.msi`, `.dmg`) ; en `.deb`/`.rpm`, l'app **notifie sans se mettre à jour** — le package manager reste maître. Packaging : `.deb`/`.rpm`/AppImage, `.dmg` (signé/notarisé quand le compte Apple existe, §15), `.msi`/NSIS.

### 10bis.3bis Feeling natif — pas un site dans une fenêtre

Objectif : l'app desktop doit être indiscernable d'une app native. Le test en review : **zéro artefact navigateur** — si un écran « sent le web », c'est un bug. Concrètement :

- **Hygiène webview** : `user-select: none` par défaut sur toute l'UI (le corps markdown d'une notification reste sélectionnable — c'est du contenu), `cursor: default` sur les contrôles (**pas de `cursor: pointer`** sur les boutons — convention web, pas desktop), pas de drag fantôme d'images/liens, zoom pinch/`Ctrl+molette` désactivé, menu contextuel navigateur supprimé.
- **Menus contextuels natifs** (API menu Tauri) : clic droit sur une row d'inbox (`ack`, `silence`, ouvrir, copier le lien), sur un canal, et sur le tray — jamais un popover HTML qui déborde de la fenêtre.
- **Typographie système** : stack `system-ui` (SF Pro / Segoe UI / fonte du DE Linux) en build Tauri — la webview doit rendre comme les apps voisines, pas comme un site. Densité `compact` (§10.2) et tailles desktop.
- **Fenêtre** : décorations **natives** par défaut (snap, resize, double-clic titre doivent marcher partout) ; état persisté (taille/position/écran) entre les lancements. macOS : titlebar `overlay` (traffic lights intégrés) avec zone de drag explicite.
- **Conventions par OS** : accélérateurs `Cmd` sur macOS / `Ctrl` ailleurs (mappés sur la command palette et les bindings §10.4) ; macOS exige la **barre de menus native** complète (About, Settings `Cmd+,`, `Cmd+Q`…) et le **badge dock** non-lus ; Windows : badge overlay taskbar ; Linux : suivi du thème du DE.
- **Interactions natives** : dialogs fichier natifs (plugin dialog), thème light/dark suivant l'OS par défaut (§10.2), focus ring visible **au clavier uniquement**, scrollbars fines type overlay, tooltips à délai court style desktop.
- **Démarrage** : fenêtre affichée seulement après le premier paint (couleur de fond = token `--background`, jamais de flash blanc) ; l'inbox rend **depuis le cache local** de `notify-client-core` avant le réseau — ouvrir l'app hors ligne montre l'état connu, pas un spinner.
- **Sobriété sonore** : les notifications OS utilisent le son système ; l'app n'embarque aucun son custom.

### 10bis.3ter Tray — l'app est un daemon d'inbox

Le tray est le mode de vie normal de l'app desktop : **fermer la fenêtre = réduire dans le tray**, l'app continue de recevoir le SSE et d'émettre les notifications OS. Quitter est une action explicite (menu tray, ou `Cmd+Q`/menu app sur macOS).

- **Icône à états** (l'icône *est* l'information, pas d'animation ni de clignotement) :

| État | Rendu |
|---|---|
| Au repos, tout lu | icône neutre |
| Non-lus | point d'accent + compteur dans le menu (pas de chiffre incrusté dans l'icône — illisible en 16 px) |
| DND / `silence` actif (§7.4) | variante lune/barrée — on voit d'un coup d'œil pourquoi ça ne sonne pas |
| Déconnecté du serveur | variante grisée + entrée de menu « reconnexion… » — jamais de popup d'erreur en boucle |

  macOS : icônes **template** (monochromes) pour suivre automatiquement clair/sombre et le rendu Retina de la barre de menus.
- **Interactions** — conventions par OS, pas un comportement unique : clic gauche = **quick panel** (§10bis.3quater) sur Windows/Linux ; sur macOS le clic ouvre le **menu** (convention menu bar extra), avec le quick panel en première entrée. « Ouvrir l'inbox » (menu et panel) amène à la fenêtre principale.
- **Menu tray** (natif, §10bis.3bis) : ligne d'état (`3 non-lus · connecté` — informative, désactivée), Ouvrir l'inbox, **DND rapide** (`1h`, `jusqu'à demain 8h`, `désactiver` — écrit côté serveur §7.4, donc propagé à tous les clients via l'événement `preference` §8.1), Rechercher les mises à jour (§10bis.3), Quitter.
- **Linux, tray non garanti** : l'icône passe par StatusNotifierItem/appindicator ; sur un DE sans tray (GNOME vanilla sans extension), **fermer = quitter** — on ne laisse jamais un processus vivant sans représentation visible. La détection est au lancement, pas une option à cocher ; l'**autostart** (§10bis.3) suit la même règle : sans tray détecté, il ouvre la fenêtre au lieu de démarrer caché.
- Le compteur non-lus du tray, du badge dock/taskbar (§10bis.3bis) et de la sidebar (§10.4) provient du **même état de `notify-client-core`** — trois rendus, une seule vérité.

### 10bis.3quater Surfaces rapides — quick panel, palette, deep links, CLI

- **Quick panel tray** : seconde fenêtre Tauri (multi-fenêtre) sans décoration, ancrée à l'icône tray (plugin positioner), fermée au blur/`Esc` : les N dernières notifications, `ack` inline, toggle DND, « ouvrir l'inbox ». Même front Vue sur une route dédiée, alimenté par le même état `notify-client-core` (§10bis.1) — le geste « centre de notifications » de l'OS : consulter/acquitter en deux secondes sans ouvrir l'app.
- **Command palette / quick switcher** : spécifiée côté front (§10.4) — c'est la même dans la fenêtre Tauri et en web. Le build Tauri lui apporte deux choses : la recherche passe par le **cache local** de `notify-client-core` (instantanée, offline) au lieu de l'API, et elle est atteignable par le raccourci global ci-dessous.
- **Raccourci global** (plugin global-shortcut) : opt-in, **non assigné par défaut** (on ne squatte pas un raccourci système), configurable dans Settings. Deux actions au choix : toggle du quick panel (défaut), ou fenêtre principale ouverte **directement sur la palette** — chercher/switcher depuis n'importe quelle app. Sous Wayland le mécanisme passe par le portal desktop (non garanti selon le compositeur) — même règle que le tray (§10bis.3ter) : détection à l'exécution, option masquée si indisponible.
- **Deep links applicatifs** : le scheme OAuth de §10bis.2 se généralise — `weebo-notify://n/<id>` (panneau détail) et `weebo-notify://c/<id>` (canal), miroir exact des permaliens web (§10.4). Clic sur une notification OS, un mail de sink (§7.1bis) ou un log CI → l'app s'ouvre au bon endroit.
- **CLI sidecar** : les packages desktop embarquent le binaire `notify` (§9), installation dans le PATH **opt-in** (installeur ou Settings). Auth **et état partagés** (§9.3bis — coffre OS, store SQLite commun, même identité device) : un seul enrôlement, une seule inbox locale ; quand l'app tourne en tray, la CLI répond depuis son cache chaud. Détail packaging par OS (feature `.msi`, commande `.dmg`, sous-paquet deb/rpm) à trancher en M5.

### 10bis.4 Android (M6)

- **Push** : UnifiedPush (§7.1). Côté serveur, l'adapter `push-unifiedpush` (§4) expose le endpoint push ; côté app, on utilise le **distributeur système** s'il est présent (ntfy…), sinon le **distributeur embarqué** de l'app maintient sa propre connexion au serveur — le chemin par défaut, 100 % self-hosted. **FCM en fallback opt-in** (variant de build séparé, jamais dans le build F-Droid).
- **Enrôlement device** (§6.2) : paire X25519 générée dans l'**Android Keystore** (StrongBox si disponible), clé publique poussée via `POST /devices`. Déchiffrement de la sealed box dans le **service de réception push** (§6.3, plugin Tauri Kotlin dédié) avant affichage — le payload en clair ne quitte jamais le device.
- Distribution : APK signé en release CI (Obtainium-friendly), dépôt F-Droid self-hosted possible plus tard ; AAB/Play Store non prioritaire.

### 10bis.5 iOS (M7)

- **Push** : APNs token-based `.p8` (§4). Le blob push suit §6.3 : sealed box, ou poke si trop gros.
- **Notification Service Extension** (Swift) : déchiffre localement avec la clé device avant affichage (§6.3). Précision sur la clé : le Secure Enclave ne manipule pas de courbes non-NIST — la paire X25519 est générée dans l'app et stockée dans le **Keychain** (`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`, access group partagé avec l'extension) : protection matérielle au repos, pas de génération in-enclave.
- Seul client non 100 % self-hosted (compte Apple Developer, §15) : distribution TestFlight puis App Store ; le build exige un runner macOS en CI (§10bis.6).

### 10bis.6 Coquille : sécurité & CI

- **Capabilities Tauri v2 minimales** par fenêtre : notification, deep-link, opener, tray, updater, positioner, global-shortcut (§10bis.3quater) — rien d'autre ; pas d'API `shell` générique, seul le sidecar `notify` (§10bis.3quater) est déclaré. Le quick panel a sa **propre capability**, encore plus réduite (notification + positioner). `opener` restreint aux URLs `http(s)` (même règle que le TUI, §9.2) ; la CSP du front (§10.3) s'applique telle quelle dans la webview.
- **Pas de code distant** : la webview charge exclusivement les assets embarqués ; les mises à jour passent par l'updater signé (desktop) ou les stores (mobile) — jamais de « hot update » du front.
- CI (§3.2) : jobs desktop sur les trois OS (artefacts de release), Android en conteneur (SDK/NDK pinnés **dans l'image**, pas via `mise` — qui n'a pas de backend Android de premier ordre, §14.4 D33), iOS sur runner macOS — le seul job qui ne tourne pas sur l'infra self-hosted (§15).

---

## 11. Configuration (GitOps)

Priorité : défauts < fichier TOML < variables d'env (`WNK_` prefix, figment). **Chaque clé de config, sans exception, est surchargeable par variable d'env** — convention `WNK_` + chemin en `__` (`[oidc] issuer` → `WNK_OIDC__ISSUER`) ; la règle est vérifiée par un test CI qui parcourt le schéma de config complet. C'est ce double concept fichier/env qui permet à un même `dev.toml` de servir compose **et** Che (§12.1) sans variante. Tout est déclaratif et versionnable :

```toml
[server]
bind = "0.0.0.0:8080"
cors_origins = ["https://notify.weebo.poc"]   # §5.1 — fermé par défaut ([])

[database]
url = "postgres://..."          # ou WNK_DATABASE__URL

[oidc]
issuer    = "https://auth.weebo.poc"
client_id = "weebo-notify"

[admin]
oidc_groups = ["platform-admins"]     # rôle plateforme (§5.4) — GitOps only, pas d'auto-promotion

[attachments]
endpoint = "https://s3.weebo.poc"     # S3-compatible : RustFS self-hosted, ou tout S3
bucket   = "weebo-notify-attachments"

[push.webpush]
vapid_key_file = "/secrets/vapid.pem"
contact = "mailto:ops@weebo.poc"      # RFC 8292 `sub` — un relais refuse une assertion sans lui (M3.5)

[push.apns]
key_file = "/secrets/apns.p8"
key_id   = "..."                      # les 10 caractères Apple assigne à cette clé .p8 — devient le `kid` du JWT
team_id  = "..."                      # devient le `iss` du JWT
topic    = "..."                      # le bundle id de l'app — envoyé en `apns-topic` sur chaque requête
# sandbox = false                     # true pour api.sandbox.push.apple.com (builds signés debug) — défaut false

[federation]                          # fédération CI (§5.3) — absent = pas d'échange, /auth/token/exchange rend 501
signing_key_file = "/secrets/federation.pem"   # Ed25519 PKCS#8 ; jamais généré au boot (un replica signerait ce qu'un autre ne vérifie pas)
# audience = "weebo-notify"           # défaut ; à changer si deux instances partagent un ingress

[sinks.smtp]                          # le relais des sinks `email` (§7.1bis) — instance-wide par nature
url  = "smtp://mail.weebo.poc:25"     # smtp:// dans le cluster, smtps:// en dehors
from = "weebo-notify <notify@weebo.poc>"
# username = "notify"
# password_ref = "env:WNK_SMTP_PASSWORD"     # mêmes formes que secret_ref
# permalink_base = "https://notify.weebo.poc" # sans lui, aucun lien n'est écrit dans le mail (§10.4)

[retention]
default_ttl = "7d"                    # défaut global, surchargeable par canal puis par message
max_ttl     = "90d"                   # clamp dur, applicable par canal

[ha]
leader_election = "postgres"          # postgres (advisory lock, défaut) | kubernetes (Lease API)

[eventbus]
backend = "postgres"                  # postgres (LISTEN/NOTIFY, défaut) | redis (pub/sub, HA/scale-out)
# redis_url = "redis://redis.weebo.poc:6379"   # requis si backend = redis (Valkey compatible)

[[trust_policy]]
issuer   = "https://forgejo.weebo.poc"
subject  = "repo:weebo/*"
channels = ["ci-*"]
scopes   = ["publish"]

# Service accounts managés (§5.2bis) — réconciliés comme les canaux ;
# leurs clés se créent via API/UI (secret montré une seule fois)
[[service_accounts]]
id           = "bot-argocd"
display_name = "ArgoCD"
description  = "Publie l'état des syncs ArgoCD"

# ───────────────────────────────────────────────
# Canaux déclaratifs (GitOps) : définition complète,
# accès par groupe OIDC, publishers CI par subject.
# ───────────────────────────────────────────────

[channels]
on_removed = "archive"        # archive | delete quand un canal managé disparaît de la config
reload     = "sighup"         # rechargement à chaud sans redémarrage

[[channels.managed]]
id               = "cluster-ovh-1"
name             = "Cluster OVH 1 — incidents"
default_ttl      = "30d"
max_ttl          = "90d"
default_priority = "high"
receipts         = true       # accusés delivered/seen activés
limits           = { publish_per_min = 120 }   # surcharge des quotas §8.2

  # Accès humains : groupe OIDC, sub ou email → scopes
  [[channels.managed.access]]
  oidc_group = "infra-admins"
  scopes     = ["publish", "subscribe", "manage"]

  [[channels.managed.access]]
  oidc_sub   = "ak-user-42"          # binding individuel par subject OIDC
  scopes     = ["subscribe"]

  [[channels.managed.access]]
  email      = "maxime@weebo.poc"    # binding individuel par email
  scopes     = ["publish", "subscribe", "manage"]

  [[channels.managed.access]]
  oidc_group = "dev-team"
  scopes     = ["subscribe"]

  [[channels.managed.access]]
  service_account = "bot-argocd"     # membership d'un service account (§5.2bis)
  scopes          = ["publish", "subscribe"]

  # Sinks sortants (v1)
  [[channels.managed.sinks]]
  type   = "webhook"
  url    = "https://hooks.weebo.poc/incidents"
  secret_ref = "env:WNK_SINK_INCIDENTS_HMAC"   # ou file:/... — résolu au démarrage

  [[channels.managed.sinks]]
  type    = "email"
  address = "oncall@weebo.poc"

  # Publishers machine : fédération JWT (Forgejo/GitHub Actions) scoped au canal
  [[channels.managed.publishers]]
  issuer  = "https://forgejo.weebo.poc"
  subject = "repo:weebo/infra:ref:refs/heads/main"
  scopes  = ["publish"]

[[channels.managed]]
id          = "ci-infra"
name        = "CI infra"
default_ttl = "48h"
receipts    = false

  [[channels.managed.access]]
  oidc_group = "dev-team"
  scopes     = ["subscribe"]

  [[channels.managed.publishers]]
  issuer  = "https://forgejo.weebo.poc"
  subject = "repo:weebo/*"
  scopes  = ["publish"]
```

**Haute disponibilité (multi-replicas)** — le bloc de référence, tout le reste du doc s'y raccroche :

- **API stateless par construction** : tout l'état vit dans PG/S3 ; pas de session serveur (§5.1), pas de sticky sessions — n'importe quel replica sert n'importe quelle requête, y compris la première d'un client qui n'a jamais parlé à ce replica.
- **SSE/WS en multi-replicas** : chaque replica tient ses propres connexions ; le fan-out passe par l'EventBus (§4 — pokes, jamais les payloads), donc **tous** les replicas apprennent chaque publication et notifient leurs clients connectés. Le failover de connexion est transparent : le client reconnecte (n'importe quel replica) avec `Last-Event-ID`, le curseur global (§8.1) garantit zéro trou et zéro doublon.
- **Backend d'EventBus selon l'échelle** (`[eventbus]`) : `postgres` par défaut (LISTEN/NOTIFY — zéro infra en plus, best-effort suffisant puisque la vérité est en DB et que le curseur rattrape tout) ; **`redis`** (pub/sub, Valkey compatible) quand le NOTIFY par transaction commence à peser — beaucoup de replicas × forte cadence de publication — pour décharger Postgres du fan-out. Même contrat de pokes, le domaine ne voit pas la différence.
- **Jobs de fond sur le leader uniquement** : purge TTL (§7.2bis), réconciliation GitOps, retries/DLQ des sinks, digests DND (§7.4). Élection au choix via `[ha]` : `postgres` (advisory lock `pg_try_advisory_lock`, défaut — zéro dépendance) ou `kubernetes` (Lease API `coordination.k8s.io`, RBAC fourni par le chart). À la perte du lock/lease, un autre replica reprend (testé sous contention, T22) ; les non-leaders servent l'API normalement.
- **Rolling updates sans coupure** : migrations **expand/contract** — chaque version N doit tourner sur le schéma de N+1 (la migration s'applique avant le rollout, job pre-hook Helm) ; `/readyz` (§13) gate l'entrée dans le service ; au SIGTERM, le replica **draine** : arrêt d'accueil de nouvelles connexions, fermeture propre des SSE (les clients re-connectent ailleurs), fin des requêtes en vol dans le `terminationGracePeriod`.
- **Chart Helm** : `PodDisruptionBudget`, anti-affinity par nœud, HPA (CPU + jauge de connexions SSE §13) — valeurs par défaut fournies, GitOps via ArgoCD.
- **SPOF restants, assumés** : **Postgres** (HA via operator — CloudNativePG/Patroni — hors périmètre applicatif, hardening plan) et le **bucket**. **Authentik down = dégradation douce, pas une panne** : les access tokens déjà émis restent valides jusqu'à leur `exp` (JWKS en cache), les API keys et jetons CI (signés par nous, §5.3) ne dépendent pas de l'IdP — seul le *login* de nouveaux users est indisponible.

**Réconciliation** : les canaux `managed` ont la config pour source de vérité — au boot et sur SIGHUP, le serveur crée/corrige le drift (accès, TTL, receipts) ; un canal retiré de la config est archivé (ou supprimé selon `on_removed`). Les canaux créés via API/UI sont marqués `dynamic` et ne sont jamais touchés par la réconciliation ; l'UI affiche le badge `managed` (lecture seule sur les champs pilotés par GitOps). Les blocs `publishers` d'un canal sont des trust policies **scopées au canal**, prioritaires sur les `[[trust_policy]]` globales.

**Ce qu'un SIGHUP adopte, et ce qu'il ne peut pas** (précisé et implémenté en M4duodetricies) : le signal relit le fichier et rejoue la réconciliation, donc `[[channels.managed]]` et `[[service_accounts]]` — les deux sections que le réconciliateur applique *à la base*. Tout le reste a été câblé dans un objet vivant au démarrage : la socket, le pool Postgres, le vérificateur OIDC, la flotte de sinks, et les `[[trust_policy]]` que l'échange de jeton détient. Ceux-là **ne bougent pas sans redémarrage**, et ce qui a changé dans le fichier est nommé en avertissement plutôt que passé sous silence — la même règle que `unhonoured` au démarrage.

**Les trust policies sont adoptées aussi, depuis M4undetricies** (D31). Elles étaient détenues *par valeur* par l'échange de jeton, donc un reload changeait un fichier et rien d'autre — et c'est pourquoi M4duodetricies avait choisi de garder l'ensemble courant plutôt que de mentir : §13 fait journaliser leurs changements par le réconciliateur (D28), et adopter la version du fichier aurait écrit « telle forge peut désormais publier » dans le journal d'audit pendant que le process continuait de la refuser. Une entrée qui date un changement de droits de la mauvaise heure est pire qu'une entrée absente, parce qu'une revue d'incident lui fait confiance. `notify_app::LivePolicies` rend l'échange réel : le fichier, le process et le journal disent maintenant la même chose, au même moment.

Déploiement : Helm chart (values GitOps via ArgoCD), migrations sqlx au démarrage (job pre-hook), secrets montés via le mécanisme du cluster (External Secrets, sealed-secrets ou SOPS selon la stack — keyring compris), images signées cosign + provenance Rekor (cohérent avec le hardening plan).

---

## 12. Environnement de développement

Objectif : **un env complet en une commande**, identique qu'on développe en local (Docker/Podman + compose) ou dans Eclipse Che (devfile). Les deux mondes exposent **les mêmes services, sur les mêmes ports, avec les mêmes credentials** → un seul `config/dev.toml` fonctionne partout, aucun `if env == che` dans le code.

### 12.1 Principe de parité compose ⟷ devfile

| Service | Rôle | Compose (`dev/compose.yaml`) | Devfile (`devfile.yaml`) | Endpoint (identique) |
|---|---|---|---|---|
| PostgreSQL 17 | DB serveur **+** DB Authentik (2 bases, 1 instance) | service `postgres` | component `postgres` | `localhost:5432` |
| RustFS | Object store S3-compatible (attachments) | service `rustfs` | component `rustfs` | S3 `localhost:9000`, console `localhost:9001` |
| `oidc-mock` (crate maison) | **OIDC du quotidien** (§12.5 — alice/bob/groupes en TOML commité) | **pas un service** : `task dev:oidc` | **pas un component** : `task dev:oidc` dans `tools` | `http://localhost:9110` |
| Authentik server | OIDC **de fidélité** (flow réel : rotation refresh, logout — §3.2 `oidc-compat`) | service `authentik-server` (profile `authentik`) | — (compose/CI uniquement) | `http://localhost:9100` |
| Authentik worker | Tâches asynchrones Authentik (blueprints, mails) | service `authentik-worker` (profile `authentik`) | — | — |
| Redis/Valkey | Broker requis par Authentik ; sert aussi à tester `eventbus-redis` (§11) | service `redis` (profiles `authentik`, `eventbus`) | component `redis` | interne `localhost:6379` |
| Mailpit | Puits SMTP pour tester le sink `email` | service `mailpit` (profile `sinks`) | — (rarement utile en Che) | UI `localhost:8025`, SMTP `localhost:1025` |
| Dex | OIDC **minimal strict** — aucune claim `groups`, le cas T29 (§3.2 `oidc-compat`) | service `dex` (profile `dex`, config rendue par `task dev:dex`) | — (CI et reproduction manuelle) | `http://localhost:5556/dex` |
| weebo-notify | Le serveur, lancé **hors conteneur** (`cargo run` / hot-reload) | — | container `tools` du parent WeeboDevImage | `localhost:8080` (web `localhost:5173`) |

Points de parité importants :
- **Trois étages d'auth** (rationale §3.2 `oidc-compat`) : `TokenVerifier` in-memory (zéro service, travail domaine) < **le mock maison** (défaut — `cargo run -p notify-oidc-mock`, aucune image, remplace authentik-server + worker + redis au quotidien) < **Authentik** (profile `authentik`, monté quand on touche au flow OIDC réel). Le mock et Authentik décrivent les **mêmes identités de test** (§12.5).
- **Le provider du quotidien n'est pas un conteneur** (ADR-0014) : c'est un binaire du workspace, lancé comme le serveur et le front. C'est ce qui fait que compose, Che et la CI partagent *un seul* provider, lisant *un seul* fichier commité (`dev/oidc-mock/identities.toml`) — plus d'image tierce à puller, plus de configuration à dupliquer dans le devfile, plus d'étape de génération à differ en CI.
- **Ports** : le mock écoute sur `9110`, Authentik garde `9100` (`AUTHENTIK_LISTEN__HTTP=0.0.0.0:9100` — 9000 est pris par RustFS, et en devfile tous les composants partagent le pod) — les deux peuvent tourner en même temps. `dev.toml` pointe sur le mock ; cibler Authentik = surcharger `WNK_OIDC__ISSUER` (§11), pas de variante de fichier.
- **Limite de parité assumée — ce qui transite par le navigateur** : en Che, le flow OIDC (redirects) atteint l'IdP via la **route publique du workspace**, pas via `localhost`. Le mock tournant dans le container `tools` comme le serveur (`:8080`) et Vite (`:5173`), Che lui ouvre une route de la même façon. Comme toute clé de config est surchargeable par env (§11), pas de fichier dédié : on exporte `WNK_OIDC_MOCK_ISSUER` (ce que le provider dit être) **et** `WNK_OIDC__ISSUER` (ce que le serveur attend) sur cette route, qui prime sur `dev.toml`. Même mécanique pour tout autre endpoint browser-facing.
- Les credentials dev sont **volontairement commités** (env de dev jetable, aucune valeur prod) — `dev/` est allowlisté dans la config gitleaks. **Exception : les credentials admin Authentik**, générés au boot et jamais commités (§12.5).

### 12.2 Arborescence

```
dev/
├── compose.yaml                 # docker compose -f dev/compose.yaml
├── .env                         # credentials dev partagés (commité, valeurs jetables)
├── .env.local                   # credentials générés (bootstrap Authentik, §12.5) — gitignoré
├── secrets/                     # matière clé générée par `task dev:setup` (§12.7) — gitignoré
│   └── vapid.pem                # clé VAPID P-256, lue par [push.webpush] de config/dev.toml
├── postgres/
│   └── init/01-databases.sql    # crée weebo_notify + authentik (user/db dédiés)
├── oidc-mock/
│   └── identities.toml          # client public weebo-notify (PKCE, redirect URIs) + alice/bob/carol
│                                # et leurs groupes — miroir du blueprint (§12.5), lu par crates/oidc-mock
├── authentik/
│   └── blueprints/
│       └── weebo-notify.yaml    # provider OIDC + app + groupes + users de test (auto-appliqué)
└── seed/
    └── seed.sh                  # buckets RustFS, migrations sqlx, blueprint Authentik (si le profile tourne)
devfile.yaml                     # racine du repo, parent WeeboDevImage + sidecars ci-dessous
config/dev.toml                  # pointe sur les endpoints du tableau §12.1
```

### 12.3 `dev/compose.yaml`

```yaml
name: weebo-notify-dev

x-authentik-env: &authentik-env
  AUTHENTIK_SECRET_KEY: dev-only-not-a-secret
  AUTHENTIK_LISTEN__HTTP: 0.0.0.0:9100
  AUTHENTIK_POSTGRESQL__HOST: postgres
  AUTHENTIK_POSTGRESQL__NAME: authentik
  AUTHENTIK_POSTGRESQL__USER: authentik
  AUTHENTIK_POSTGRESQL__PASSWORD: authentik
  AUTHENTIK_REDIS__HOST: redis
  AUTHENTIK_BOOTSTRAP_EMAIL: admin@weebo.local
  AUTHENTIK_BOOTSTRAP_PASSWORD: ${AUTHENTIK_BOOTSTRAP_PASSWORD}   # généré par task dev:auth → dev/.env.local (gitignoré, §12.5)
  AUTHENTIK_BOOTSTRAP_TOKEN: ${AUTHENTIK_BOOTSTRAP_TOKEN}         # idem
  AUTHENTIK_ERROR_REPORTING__ENABLED: "false"
  AUTHENTIK_DISABLE_UPDATE_CHECK: "true"

services:
  postgres:
    image: postgres:17-alpine
    ports: ["5432:5432"]
    environment:
      POSTGRES_USER: weebo
      POSTGRES_PASSWORD: weebo
      POSTGRES_DB: weebo_notify
    volumes:
      - pg-data:/var/lib/postgresql/data
      - ./postgres/init:/docker-entrypoint-initdb.d:ro
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U weebo"]
      interval: 5s
      retries: 10

  rustfs:
    image: rustfs/rustfs:latest
    ports: ["9000:9000", "9001:9001"]   # S3 API / console web
    environment:
      RUSTFS_ACCESS_KEY: weebo-dev
      RUSTFS_SECRET_KEY: weebo-dev-secret
      RUSTFS_VOLUMES: /data
      RUSTFS_CONSOLE_ENABLE: "true"
    volumes:
      - rustfs-data:/data

  # Pas de service OIDC ici : depuis ADR-0014 le provider du quotidien est un binaire
  # du workspace (`task dev:oidc`, port 9110), lancé comme le serveur et le front.

  redis:
    image: valkey/valkey:8-alpine
    profiles: [authentik, eventbus]     # broker Authentik ; seul, il sert à tester eventbus-redis (§11)

  authentik-server:
    image: ghcr.io/goauthentik/server:2025.6
    command: server
    profiles: [authentik]
    ports: ["9100:9100"]
    environment: *authentik-env
    volumes:
      - ./authentik/blueprints:/blueprints/weebo:ro
    depends_on:
      postgres: { condition: service_healthy }
      redis: { condition: service_started }

  authentik-worker:
    image: ghcr.io/goauthentik/server:2025.6
    command: worker
    profiles: [authentik]
    environment: *authentik-env
    volumes:
      - ./authentik/blueprints:/blueprints/weebo:ro
    depends_on:
      postgres: { condition: service_healthy }
      redis: { condition: service_started }

  mailpit:
    image: axllent/mailpit:latest
    profiles: [sinks]
    ports: ["8025:8025", "1025:1025"]

volumes:
  pg-data:
  rustfs-data:
```

> Versions d'images (RustFS, Authentik 2025.x, Valkey) à figer lors de la mise en place réelle — le compose ci-dessus fixe le contrat, pas les digests. Même chose pour l'image Dex utilisée par le job `oidc-compat` (§3.2), qui vit côté CI et non dans ce compose.

### 12.4 `devfile.yaml` (Eclipse Che)

Le devfile hérite du parent `weebo-base`/`WeeboDevImage` (container `tools` avec mise, rust, node...) et ajoute les mêmes services en **sidecars du workspace pod** :

```yaml
schemaVersion: 2.3.0
metadata:
  name: weebo-notify
parent:
  uri: https://raw.githubusercontent.com/batleforc/WeeboDevImage/main/che-min-mise/devfile.yaml
components:
  - name: postgres
    container:
      image: postgres:17-alpine
      memoryLimit: 512Mi
      env:
        - { name: POSTGRES_USER,     value: weebo }
        - { name: POSTGRES_PASSWORD, value: weebo }
        - { name: POSTGRES_DB,       value: weebo_notify }
        - { name: PGDATA,            value: /data/pgdata }
      volumeMounts:
        - { name: pg-data, path: /data }
      endpoints:
        - { name: postgres, targetPort: 5432, exposure: internal }

  - name: rustfs
    container:
      image: rustfs/rustfs:latest
      memoryLimit: 512Mi
      env:
        - { name: RUSTFS_ACCESS_KEY, value: weebo-dev }
        - { name: RUSTFS_SECRET_KEY, value: weebo-dev-secret }
        - { name: RUSTFS_VOLUMES,    value: /data }
      volumeMounts:
        - { name: rustfs-data, path: /data }
      endpoints:
        - { name: s3,            targetPort: 9000, exposure: internal }
        - { name: rustfs-console, targetPort: 9001, exposure: public, protocol: https }

  - name: redis
    container:
      image: valkey/valkey:8-alpine
      memoryLimit: 128Mi
      endpoints:
        - { name: redis, targetPort: 6379, exposure: internal }

  # Pas de component OIDC (ADR-0014) : le provider tourne dans `tools` via la commande
  # run-oidc, comme le serveur et Vite — Che lui ouvre une route de la même façon, et
  # `dev/oidc-mock/identities.toml` est lu depuis les sources du projet. C'est ce qui
  # supprime aussi l'ancienne étape `gen:devfile` (Che ne monte pas de fichiers hôte,
  # il fallait donc inliner cette config en variable d'env et la differ en CI).

  - name: pg-data
    volume: { size: 2Gi }
  - name: rustfs-data
    volume: { size: 2Gi }

commands:
  - id: dev-setup
    exec: { component: tools, commandLine: "task dev:setup", workingDir: "${PROJECT_SOURCE}" }
  - id: dev-seed
    exec: { component: tools, commandLine: "task dev:seed", workingDir: "${PROJECT_SOURCE}" }
  - id: run-oidc
    exec: { component: tools, commandLine: "task dev:oidc", workingDir: "${PROJECT_SOURCE}" }
  - id: run-server
    exec: { component: tools, commandLine: "task dev:run", workingDir: "${PROJECT_SOURCE}" }
  - id: run-web
    exec: { component: tools, commandLine: "task dev:web", workingDir: "${PROJECT_SOURCE}" }
```

Notes Che :
- Tous les composants partagent le pod → tout est joignable en `localhost`, exactement comme le mapping de ports du compose.
- **Authentik n'est plus embarqué dans le devfile** : le workspace tourne sur le mock (un process, zéro Mi de sidecar, au lieu de ~1,5 Gi Authentik + worker) ; la fidélité IdP se joue en compose local (profile `authentik`) ou en CI (`oidc-compat`, §3.2) — c'est le principal gain de la stratégie à trois étages (§12.1).
- **Le provider OIDC n'est plus un sidecar non plus** (ADR-0014) : `run-oidc` le lance dans `tools`, donc Che lui ouvre une route comme il le fait pour `:8080` et `:5173`.
- Redis reste en sidecar léger pour tester `eventbus-redis` (§11).
- Mailpit n'est pas embarqué dans le devfile (rarement utile en Che) ; le sink email se teste côté compose (profile `sinks`).
- Les exports `WNK_OIDC_MOCK_ISSUER` / `WNK_OIDC__ISSUER` (§12.1) se font dans le terminal du workspace, sur la route que Che propose pour `:9110` — plus besoin de faire fusionner un env par le devfile.

**Ce que l'image `tools` ne porte pas encore, et qui bloque M5.** Tauri v2 se lie sous Linux à une pile GTK/WebKit — `webkit2gtk-4.1`, `javascriptcoregtk-4.1`, `libsoup-3.0`, `gtk+-3.0`, plus `libayatana-appindicator3-dev` pour le tray (§10bis.3ter), `librsvg2-dev` et `patchelf` pour l'empaquetage. Aucune des quatre premières n'est présente dans `che-min-mise`, et **rien dans ce dépôt ne peut y remédier** : ce sont des bibliothèques système, `mise` n'en porte pas (il livre des outils, pas du C), et les installer demande root — ce qu'un workspace Che ne donne pas, à dessein.

Le correctif appartient donc à l'**image**, c'est-à-dire à `batleforc/WeeboDevImage`, ce qui est aussi la règle de §3 : une amélioration générique remonte dans le template plutôt que d'être contournée ici. Une fois les paquets dans l'image, `devfile.yaml` n'a rien à changer — le container `tools` hérite du parent, donc un tag reconstruit est tout le correctif.

En attendant, la question se pose à la machine plutôt qu'au document : `task tauri:doctor` liste ce qui manque et où le réparer. C'est délibérément une commande et pas un paragraphe — la réponse diffère par machine et change au premier rebuild d'image, donc une phrase écrite ici serait fausse le jour où quelqu'un corrige son environnement, et un contrôle qui s'exécute ne l'est jamais.

**Et aucun sidecar ne s'y substitue** — vérifié en M4tervicies plutôt que supposé, parce que la tentation est réelle : ce dépôt planifie Postgres, RustFS et Valkey sans qu'aucun moteur de conteneurs n'existe ici, donc un composant portant la pile GTK semble possible. Il ne l'est pas. Un DevWorkspace applique sa security context au **pod entier**, donc aucun conteneur n'y est root et `apt-get` échoue dans un sidecar exactement comme il échoue dans `tools`. Planifier un conteneur et être root dedans sont deux capacités différentes, et celle-ci demande la seconde. Le correctif reste amont, et son diff est prêt dans `docs/upstream/che-min-mise-tauri.md`.

**Ce que le devfile porte quand même, depuis M4tervicies.** Deux composants, pour deux dettes qui n'avaient pas ce problème :

- **Dex** (D7), en `sleep infinity` : sa configuration est rendue par `task dev:dex` et n'existe pas au démarrage du pod, donc un conteneur qui la réclamerait tout de suite bouclerait en crash à chaque ouverture de workspace. La commande `run-dex` démarre le process une fois le fichier écrit. Authentik reste dehors pour la raison qui n'a pas changé — ~1,5 Gi ;
- **un navigateur** (`che-browser`, l'image voisine dans le même dépôt amont) : Chrome sous Xvfb, avec CDP et WebDriver en interne et **noVNC sur une route publique**. C'est ce qui manquait à D19, D21 et D26, tous les trois écrits « jamais vus » : la suite web tourne sous jsdom, et jsdom répond sur la logique — pas sur l'allure d'une palette, pas sur une image qui décale sa ligne en chargeant. Le partage est délibéré : le navigateur vit dans ce composant, ce qui le pilote vit dans `tools`, et tout se joint sur `localhost` puisque le pod est partagé. Il **monte les sources**, contrairement au devfile de référence amont : un screenshot CDP revient sur le fil et n'a besoin de rien, mais un téléchargement Chrome, une trace Playwright, un artefact écrit d'un côté et relu de l'autre ou un `dist/` ouvert en `file://` réclament tous un répertoire que les deux conteneurs voient — sans quoi chacun d'eux se paie son propre transport.

**Et ce qui le pilote, depuis M4quatervicies** : `dev/browser/cdp.mjs`, exposé par le namespace `browser:` du Taskfile — `check`, `open URL=…`, `shot OUT=…`, `eval JS=…`, `logs`. **Zéro dépendance** : Node 24 livre un `WebSocket` global et CDP est du JSON dessus, donc le client fait soixante lignes ; Playwright ou puppeteer téléchargeraient un navigateur que ce pod a déjà, pour finir par s'y connecter en CDP de toute façon. Il pilote **la cible existante** et non un onglet à lui : la route noVNC est là pour qu'un humain regarde, et un pilote headless déplacerait hors champ exactement ce qu'on veut voir. Hors Che, ces tâches ne font rien plutôt que d'échouer — `dev/compose.yaml` n'a pas de navigateur, et regarder une page sur un portable est le travail du navigateur du portable.

**Et ce que ce sidecar portait sans qu'on le sache, depuis M4sexvicies** : un `dbus-daemon`. Il est dans l'image `che-browser` — Chrome en dépend — et il n'est nulle part ailleurs dans ce pod. Comme les deux conteneurs montent les sources, une socket de bus de session posée sur ce chemin partagé est atteignable depuis `tools` : c'est `task desktop:bus`, et c'est la moitié qui manquait à **D19**, la seule des trois dettes « jamais vues » qu'un navigateur ne pouvait pas fermer — `--notify-send` vise un démon, pas une page. L'autre moitié, un process qui possède `org.freedesktop.Notifications`, ne s'installe pas ici (ni `dunst`, ni `mako`, ni `dbus-daemon` dans mise, et le pod est sans root) : `xtask notifyd` répond lui-même aux quatre méthodes de la spécification. Même arbitrage que `cdp.mjs` contre Playwright, avec un avantage en plus — un stub qu'on possède peut **affirmer** ce qui est arrivé, ce qu'un vrai démon ne sait pas faire.

Deux instruments, deux questions : ces tâches pour une question qui a une réponse (une capture, un `aria-activedescendant`), la route noVNC pour un humain qui veut simplement *voir*. C'est le second qui a rendu D21 — un curseur clavier et un survol souris rendus dans le même gris n'est pas une question qu'on sait poser avant de l'avoir vue.

### 12.5 Identités de test — mock OIDC & blueprint Authentik

Règle : le mock et Authentik décrivent les **mêmes identités** (users, groupes, client) — tests E2E et manipulations manuelles sont indifférents au provider, et la matrice `oidc-compat` (§3.2, T29) rejoue la même suite sur les deux.

**Mock (`dev/oidc-mock/identities.toml`, commité)** — la source du quotidien, lue par le crate `oidc-mock` (ADR-0014) :
- `[client]` : client **public** `weebo-notify`, Authorization Code + PKCE, mêmes redirect URIs que le blueprint (dont les callbacks Tauri dès M5) ; issuer `http://localhost:9110`, surchargeable par `--issuer` / `WNK_OIDC_MOCK_ISSUER` (Che, §12.1).
- `[[users]]` : `alice` (groups `["infra-admins", "platform-admins"]`), `bob` (groups `["dev-team"]`), `carol` (**aucun groupe** — le cas T29), mot de passe `weebo-dev` — claims `groups` émis dans l'access token, comme Authentik, et **claim absent** plutôt que tableau vide quand il n'y a pas de groupe.
- Un fichier, un format (TOML, celui du reste du projet), lu à l'identique en compose, en Che et en CI — c'est ce que le format YAML propre au mock tiers empêchait.

**Blueprint Authentik (`dev/authentik/blueprints/weebo-notify.yaml`)** — la référence de fidélité, versionnée :

- **Provider OAuth2/OIDC** `weebo-notify` : Authorization Code + PKCE, client public, redirect URIs `http://localhost:8080/auth/callback` et `http://localhost:5173/auth/callback` — à compléter dès M5 avec les callbacks Tauri (loopback `http://127.0.0.1:*` + scheme `weebo-notify://oauth/callback`, §10bis.2) —, claims `groups` inclus dans l'access token.
- **Application** `weebo-notify` liée au provider.
- **Groupes** `infra-admins`, `dev-team` (alignés sur les exemples de canaux §11).
- **Users de test** : `alice` (infra-admins), `bob` (dev-team), mot de passe `weebo-dev` — en plus du bootstrap `akadmin`.

**Credentials admin Authentik — générés au boot, jamais commités** :
- **Compose** (seul monde où Authentik tourne en dev, §12.4) : au premier `task dev:auth`, génération de `AUTHENTIK_BOOTSTRAP_PASSWORD`/`_TOKEN` aléatoires dans `dev/.env.local` (gitignoré, chargé par le compose), affichés une fois dans la console. Le blueprint est appliqué par `task dev:seed` via l'API (`AUTHENTIK_BOOTSTRAP_TOKEN`) quand le profile tourne — le montage `/blueprints` du compose n'est qu'un raccourci.
- Les users de test (`alice`/`bob`, mot de passe `weebo-dev`) restent commités — dans le mock comme dans le blueprint : comptes sans privilège d'IdP jetables, et l'ergonomie locale (se loguer à la main) justifie un mot de passe fixe. **Dex fait exception** (§3.2) : CI-only, ses mots de passe sont générés au boot du job depuis un template — rien de fixe à commiter.

Le `config/dev.toml` correspondant — il pointe sur le **mock** ; cibler Authentik = surcharge d'env, jamais une variante de fichier (§12.1) :

```toml
[oidc]
issuer    = "http://localhost:9110"    # mock maison, `task dev:oidc` (défaut dev)
                                       # Authentik (task dev:auth) : WNK_OIDC__ISSUER=http://localhost:9100/application/o/weebo-notify/
client_id = "weebo-notify"

[database]
url = "postgres://weebo:weebo@localhost:5432/weebo_notify"

[attachments]
endpoint   = "http://localhost:9000"
bucket     = "weebo-notify-attachments"
access_key = "weebo-dev"
secret_key = "weebo-dev-secret"

```

> En Che, on exporte `WNK_OIDC__ISSUER` (route publique du workspace), qui prime sur ce fichier — cf. §12.1. Le fichier reste identique dans les deux mondes.

### 12.6 Workflow quotidien (Taskfile, namespace `dev:`)

```yaml
dev:setup:   # tout ce qu'un clone frais doit créer avant de lancer quoi que ce soit (§12.7)
             task dev:env && task dev:vapid
dev:env:     # bootstrap Authentik dans dev/.env.local (§12.5), si absent
dev:vapid:   cargo run -p xtask -- vapid-key --output dev/secrets/vapid.pem   # si absente
dev:up:      docker compose -f dev/compose.yaml --env-file dev/.env.local up -d --wait   # pg + rustfs
dev:oidc:    cargo run -p notify-oidc-mock -- --identities dev/oidc-mock/identities.toml  # :9110, provider du quotidien (ADR-0014)
dev:auth:    # génère dev/.env.local si absent (bootstrap Authentik, §12.5) puis :
             docker compose -f dev/compose.yaml --env-file dev/.env.local --profile authentik up -d --wait
             # puis : WNK_OIDC__ISSUER=http://localhost:9100/application/o/weebo-notify/ task dev:run
dev:core:    docker compose -f dev/compose.yaml --env-file dev/.env.local up -d --wait postgres rustfs   # sans OIDC (TokenVerifier in-memory)
dev:down:    docker compose -f dev/compose.yaml --env-file dev/.env.local --profile "*" down
dev:reset:   docker compose -f dev/compose.yaml --env-file dev/.env.local --profile "*" down -v
dev:seed:    dev/seed/seed.sh    # migrations sqlx + bucket RustFS + blueprint Authentik (skip propre si le profile ne tourne pas)
dev:run:     cargo run -p notify-server -- --config config/dev.toml
dev:web:     npm --prefix web run dev
dev:logs:    docker compose -f dev/compose.yaml --env-file dev/.env.local logs -f
```

- **Local** : `task dev:up && task dev:seed` (conteneurs, **~20 s**) puis `task dev:oidc` et `task dev:run` dans deux terminaux — le provider est un process long, comme le serveur et le front. `task dev:auth` monte Authentik quand on touche au flow OIDC réel (§3.2 `oidc-compat` reste le filet en CI).
- **Che** : le workspace démarre les sidecars tout seul ; `task dev:seed` puis les commandes `run-oidc` et `run-server` du devfile. `dev:up`/`dev:down` détectent `$DEVWORKSPACE_ID` et no-opent proprement.
- `dev:seed` est **idempotent** (rejouable après `dev:reset` ou sur un workspace recréé).
- Les tests E2E (testcontainers) restent autonomes : ils ne dépendent pas de cet env, qui sert au run interactif.

### 12.7 `task dev:setup` — ce qu'un clone ne peut pas contenir

Deux choses ne peuvent pas être dans le dépôt et doivent pourtant exister avant que quoi que ce soit tourne : le bootstrap Authentik (§12.5) et, depuis M3.5, la **clé VAPID** que `[push.webpush]` lit. `task dev:setup` les crée, et c'est le seul point d'entrée — `dev:up`, `dev:core`, `dev:auth`, `dev:sinks` et `dev:run` en dépendent, et `task init` le termine.

Le raisonnement est le même que celui de `dev:seed` : **une étape de setup qu'il faut penser à faire est une étape que quelqu'un ne fera pas**, et le symptôme atterrit ailleurs — ici, un serveur qui refuse de démarrer sur une clé absente, ce qui est le bon comportement (§14.3) mais une mauvaise première impression. Chaque étape porte son propre `status:`, donc la relancer est gratuit et `dev:up` peut en dépendre sans coûter une seconde.

Trois décisions, notées parce qu'elles se relisent mal :

- **la clé est générée par `cargo run -p xtask -- vapid-key`, pas par `openssl`.** Ce workspace a refusé OpenSSL comme dépendance de build en M1 (toolchain C, cross-compilations Tauri) ; l'assumer comme *outil présent sur la machine* est le même pari sous un autre nom, et une image Che qui ne l'embarque pas transformerait `dev:setup` en question de support. Ce que cargo sait lancer, tous les mondes visés savent le lancer. En prime, le générateur affiche l'`applicationServerKey` — ce qu'`openssl` ne fait pas en une commande, et c'est la valeur que quelqu'un qui câble le push veut voir ;
- **elle refuse d'écraser une clé existante** sans `--force`. Une clé VAPID régénérée n'échoue pas bruyamment : le serveur démarre, le relais accepte l'assertion, et toutes les subscriptions prises sous l'ancienne cessent d'être livrables — en silence, sur la machine de quelqu'un d'autre ;
- **`dev/secrets/` est gitignoré et *n'est pas* allowlisté pour gitleaks.** C'est de la vraie matière clé, pas une valeur jetable comme `dev/.env` : l'exception de §12.1 (« les credentials de dev sont commités exprès ») s'arrête ici, et pour la raison exacte donnée en M3.5 — une allowlist écrite pour une clé de développement couvrirait aussi celle de production qui atterrirait au même endroit.

---

## 13. Observabilité

- **Traces** : `tracing` + `tracing-opentelemetry`, export OTLP ; spans sur chaque use case (publish → fanout → push transport), propagation W3C traceparent acceptée en entrée (une CI peut lier sa trace à la notification).
- **Metrics** : exporter Prometheus `/metrics` — `notifications_published_total{channel}`, `push_delivery_duration_seconds{transport}`, `push_failures_total{transport,reason}`, `sync_backlog_size`, `realtime_connections{transport="sse"|"ws"}` (jauge — sert aussi à l'HPA §11), `sink_dlq_depth{channel}` (la DLQ métriquée de §7.1bis).
- **Logs** : JSON structurés, jamais de payload.
- **Audit log** : les actions d'administration (création/révocation d'API keys, de devices et de service accounts, membership — self-join d'un `platform-admin` compris (§5.4) —, trust policies, requeue DLQ) sont journalisées dans une table **append-only** et émises en logs structurés `audit=true` — flux séparé des logs techniques : c'est celui qu'on rejoue après un incident. Browsable via `GET /admin/audit` (§8) dans la console plateforme (§10.4bis).
- **Qui, quand un fichier agit** (M4septvicies, D28) : les changements de trust policy, de canal managé et de service account managé sont journalisés par le réconciliateur, dont l'acteur est `{"kind":"configuration"}` et non un principal. §5 en compte quatre et les fait tous *authentifiés* ; le réconciliateur n'est aucun des quatre, et c'est précisément pourquoi cette promesse est restée sans émetteur. L'acteur est élargi **dans le port d'audit seul** : un cinquième `Principal` serait représentable partout où un principal l'est — une adhésion, une publication — et chaque `match` sur les quatre gagnerait un bras qui veut dire « ne peut pas arriver ».
- **Une entrée par changement, jamais par passe.** Le réconciliateur applique le fichier entier toutes les soixante secondes : il sait ce qu'il applique et jamais ce qui a *bougé*. `managed_declarations` garde la **déclaration** telle qu'appliquée — pas l'entité produite, qui dérive quand un manager édite ce qu'il a le droit d'éditer — et la passe suivante en fait un diff. Une instance stable n'écrit donc rien, ce qui est la condition pour que ce journal reste lisible.
- Health : `/healthz`, `/readyz` (PG, S3, JWKS caches — + Redis quand `eventbus-redis` est actif : un replica sans fan-out ne doit pas se déclarer prêt).

---

## 14. Roadmap proposée

Le frontend web est un client de premier rang : il entre dès M1 et progresse à chaque milestone ; les milestones Tauri (desktop puis Android) suivent en réutilisant ce front.

0. **M0 — Socle** : bootstrap depuis `weebo-base` (mise, Taskfile, cocogitto/hooks, double forge), env de dev compose/devfile complet (§12), squelette workspace cargo + CI `check`/`contract`.
1. **M1 — Cœur + web v0** : domaine + PG + API publish/upsert + SSE/WS + OIDC + API keys + CLI publish. TTL/purge. Côté web : shell shadcn-vue (§10), login OIDC, inbox temps réel (SSE), panneau détail/timeline.
2. **M2 — Sync, offline & TUI complet** : sync offline + badge retard + compaction, CLI/TUI complet (mode `:` §9.2). Côté web : gestion devices, profils (§2.4), admin canaux, API keys & service accounts (§5.2bis).
3. **M3 — Push web & PWA** : Web Push (RFC 8291 seul, §6.2 — aucune crypto applicative), PWA installable (offline) ; receipts (UI « vu par 3/5 ») ; DND (§7.4 — quiet hours, exceptions, digest de sortie).
4. **M4 — Fédération CI & plateforme** : token exchange, trust policies, intégration Forgejo Actions (+ dogfooding CI → weebo-notify) ; UI admin trust policies ; rôle `platform-admin` (§5.4) + console plateforme v1 (§10.4bis — DLQ, audit log, réconciliation, inventaire).
5. **M5 — Tauri desktop** (§10bis.3) : empaquetage du front existant, notifications natives OS (SSE direct — toujours sans clé device), tray + quick panel (§10bis.3ter/quater), updater signé, deep links/permaliens, store partagé et CLI sidecar (§9.3bis) ; raccourci global en option de fin de milestone.
6. **M6 — Tauri Android** (§10bis.4) : premier besoin de re-chiffrement — enrôlement X25519 (Android Keystore), sealed box à la livraison, endpoint push UnifiedPush côté serveur + distributeur embarqué dans l'app.
7. **M7 — Tauri iOS** (§10bis.5) : APNs + Notification Service Extension de déchiffrement (dépendance compte Apple Developer, §15).

**M4bis — Sinks sortants**, inséré entre M4 et M5 : §7.1bis de bout en bout — webhook signé, email SMTP, file de retries leader-only, DLQ et son onglet de console (§10.4bis). Il n'apparaissait dans aucun milestone de la liste ci-dessus, ce que la fin de M4 a nommé plutôt que passé sous silence ; il tombe ici parce que M5 est bloqué par D13 et parce que la console de M4 avait un quatrième objet qui n'existait pas.

**M4quater — Traces**, dans la foulée : l'autre puce de §13, `tracing-opentelemetry` et l'export OTLP, plus la propagation W3C `traceparent` en entrée. Elle ferme §13 — après quoi la section n'a plus de puce qui attend son incrément, ce qui était toute la raison de couper M4ter et M4quater en deux plutôt que d'annoncer « l'observabilité » d'un bloc.

**M4ter — Metrics**, juste après : la puce « Metrics » de §13, c'est-à-dire l'exporteur `/metrics` et les six familles qu'il publie. Elle n'appartenait à aucun milestone non plus — §13 en entier était supposé arriver « quelque part », ce qui est la façon dont l'observabilité n'arrive jamais — et la fin de M4bis l'a nommée en refusant d'écrire une jauge derrière un exporteur inexistant (D15). Il tombe ici pour la même raison que M4bis : M5 reste bloqué par D13, et une file de retries qu'on ne peut mesurer que dans une console est une file dont personne n'est réveillé. Il ne prend **que** la puce Metrics ; les traces OTLP de la même section restent dehors, et sont écrites en D16 plutôt que sous-entendues.

### 14.1 État d'avancement

> Mis à jour au fil de l'eau. « écrit » veut dire *dans l'arbre de travail et testé* ;
> « commité » veut dire *dans l'historique*. La distinction compte : un incrément non
> commité n'a pas passé les hooks (`cog verify`, `task recu`, `task lint`, gitleaks).

| Milestone | État |
|---|---|
| **M0 — Socle** | ✅ commité (`240ff36`) |
| **M1 — Cœur + web v0** | ✅ commité (`9cd9414`, `aef4844`) — détail ci-dessous |
| **M2 — Sync, offline & TUI** | ✅ commité (`e6a6b2e`, `731e795`) — les huit incréments, les quatre manques, puis D1 et D3 |
| **M3 — Push web & PWA** | ✅ **écrit** — M3.1 à M3.4 commités (`731e795`), M3.5 commité (`3098ebc`), M3.6 et M3.7 écrits — détail ci-dessous |
| **M4 — Fédération CI & plateforme** | 🟡 M4.1 à M4.6 écrits ; M4.7 écrit mais jamais joué contre une vraie forge — détail ci-dessous |
| **M4bis — Sinks sortants** | ✅ **écrit** — le milestone que §14.1 disait devoir être placé avant M5 ; les cinq incréments, T17 de bout en bout — détail ci-dessous |
| **M4ter — Metrics** | ✅ **écrit** — la puce « Metrics » de §13 : le port, l'exporteur, les six familles et les endroits qui les alimentent ; solde D15 — détail ci-dessous |
| **M4quater — Traces** | ✅ **écrit** — la puce « Traces » de §13 : OTLP/HTTP, quatre spans, `traceparent` accepté en entrée ; solde D16 — détail ci-dessous |
| **M4quinquies — CSP** | ✅ **écrit** — la quatrième porte de §10.3, qui n'existait nulle part : la politique voyage avec le bundle (ADR-0019). Relève le plancher qu'ADR-0016 disait plus faible que supposé — D9 reste ouverte — et ouvre D17 pour ce qu'un `<meta>` ne peut pas porter — détail ci-dessous |
| **M4sexies — `GET /channels/{id}`** | ✅ **écrit** — la dernière route de §8 que ce build ne servait pas, trouvée en diffant la liste de §8 contre `openapi.json` ; solde D18 — détail ci-dessous |
| **M4septies — le diff §9 et §11** | ✅ **écrit** — la même méthode appliquée à la CLI et à la configuration : une divergence corrigée, une phrase de §9.1 rendue vraie, deux manques nommés (D19, D20), et §11 propre — détail ci-dessous |
| **M4octies — les portes du diff** | ✅ **écrit** — T23 écrit pour de bon (test généré sur tout le schéma) et le contrôle de routes rendu **conscient des méthodes**, celui qui aurait attrapé D18 ; §2.3 et §7 diffés, propres — détail ci-dessous |
| **M4nonies — T28** | ✅ **écrit** — les deux moitiés que §3.4 promettait et qu'aucun test ne tenait : un détenteur **tué** (pas relâché) et un store supprimé — détail ci-dessous |
| **M4decies — T24** | ✅ **écrit** — le scénario migrations, sur une base créée pour lui : premier boot construit le schéma, second ne rejoue rien — détail ci-dessous |
| **M4undecies — le mode `:` complet** | ✅ **écrit** — complétion depuis l'arbre clap et historique persisté (§9.2) ; solde D20 — détail ci-dessous |
| **M4duodecies — la suite `critical` tient enfin ses 30** | ✅ **écrit** — quatre scénarios écrits mais hors profil, et **T15 qui n'était appliqué nulle part** : `MAX_PAYLOAD_BYTES` était une constante que rien ne lisait — détail ci-dessous |
| **M4terdecies — la palette `Cmd+K`** | ✅ écrit ici, **vue en M4quatervicies** — §10.4 de bout en bout : recherche serveur, switcher, actions ; cette ligne a lu « non vu » bien après que D21 avait fermé, parce que personne n'était revenu corriger le tableau une fois le navigateur arrivé. Solde D21 — détail ci-dessous |
| **M4quaterdecies — `--notify-send`** | ✅ écrit ici, **vu en M4sexvicies** — le relais desktop de §9.2 et sa règle DND ; son envoi n'a été exécuté contre un vrai démon que deux incréments plus tard (D19), ce qui est la raison pour laquelle cette ligne a lu « non vu » si longtemps. Ouvre D22 — détail ci-dessous |
| **M4quindecies — `muted_by_dnd`** | ✅ **écrit** — la règle DND répondue une fois par le serveur et portée par l'événement ; la copie de la CLI supprimée ; solde D22 — détail ci-dessous |
| **M4sedecies — le web lit la réponse** | ✅ **écrit** — la troisième copie de la règle DND supprimée ; il n'en reste qu'une, celle du domaine ; ferme D22 — détail ci-dessous |
| **M4septdecies — le plafond de connexions** | ✅ **écrit** — la ligne de §8.2 que rien ne lisait : 20 connexions SSE/WS par principal, tenues par un garde qui les rend ; ouvre D23 (portée par replica) — détail ci-dessous |
| **M4octodecies — `limits` par canal** | ✅ **écrit** — la phrase d'en-tête de §8.2 : la surcharge par canal, parsée depuis M1 et lue par personne, va maintenant jusqu'au compteur ; ouvre D24 (la moitié « par API key ») — détail ci-dessous |
| **M4novodecies — la purge des pièces jointes** | ✅ **écrit, et vérifié contre une vraie base** — une fuite de stockage sans borne : `AttachmentStore::delete` se documentait comme appelé « quand sa notification est purgée » et n'avait qu'un appelant, le remplacement d'avatar ; ouvre D25 (les uploads jamais référencés) — détail ci-dessous |
| **M4vicies — le registre des presigns** | ✅ **écrit, migration jouée** — l'autre moitié : ce que la base n'avait jamais vu est enregistré à la signature et collecté après un jour ; **solde D25** — détail ci-dessous |
| **M4unvicies — `GET /attachments/{id}`** | ✅ **écrit, vérifié contre une vraie base** — les pièces jointes étaient en écriture seule : `presign_download` existait, était implémenté, et n'avait aucun appelant ni aucune route ; ouvre D26 (le web ne les affiche toujours pas) — détail ci-dessous |
| **M4duovicies — le vocabulaire d'audit** | ✅ **bug corrigé, prouvé puis reproduit contre une vraie base** — deux actions que ce build écrit n'étaient pas dans la liste que le lecteur accepte : **un `PATCH /channels/{id}` rendait `GET /admin/audit` illisible en entier**. Le champ est devenu un type, donc la classe entière disparaît ; ouvre D28 — détail ci-dessous |
| **M4tervicies — ce que le devfile pouvait porter** | ✅ **écrit, partiellement vérifié** — trois dettes tenues pour « bloquées par l'environnement » ne l'étaient pas toutes : un devfile accepte des sidecars (§12.4). Dex entre, un **navigateur** entre — ce qui manquait aux trois dettes « jamais vues » —, les deux Containerfiles que D17 réclamait existent, et D13 reste bloquée pour une raison qui n'est pas celle-là ; deux bugs préexistants trouvés en rejouant les portes — détail ci-dessous |
| **M4quatervicies — le navigateur regarde** | ✅ **vu, et c'est le mot qui compte** — le sidecar de M4tervicies avait un navigateur et personne pour le piloter ; `dev/browser/cdp.mjs` et `task browser:` ferment cette moitié. Puis on a regardé : D21 et D26 soldées, **D7 soldée en trouvant sa jambe rouge**, et deux défauts que des mois de tests verts n'avaient pas vus — chaque ligne d'inbox affichait un ULID là où §2.4 promet un nom, et l'icône de la PWA n'était pas du XML valide — détail ci-dessous |
| **M4quinvicies — le badge non-lus** | ✅ **soldé, et la décision de modèle prise plutôt que contournée** — D27 attendait qu'on tranche « non lu » avant d'écrire du code, et elle avait raison : `receipts_enabled` vaut `false` par défaut, donc lire l'opt-in de §7.3 comme « ne rien écrire » donnait un badge qui ne redescend jamais sur un canal ordinaire. L'opt-in gouverne **qui est mis au courant** — un `seen` est désormais écrit partout, et diffusé au seul lecteur là où le canal ne collecte rien. `GET /unread`, le compte côté serveur, la sidebar web et celle du TUI ; le tray et le dock restent à M5 faute de coquille Tauri — détail ci-dessous |
| **M4sexvicies — le relais desktop, vu** | ✅ **soldé — la troisième « jamais vue » fermée, et par le sidecar qu'on croyait hors sujet** — M4quatervicies avait écrit que D19 était la seule des trois qu'un navigateur n'aidait pas. C'était vrai du navigateur et faux de son image : `dbus-daemon` y est, les deux conteneurs montent les sources, donc une socket de bus sur ce chemin partagé se joint depuis `tools`. `xtask notifyd` répond aux quatre méthodes de `org.freedesktop.Notifications`, `task desktop:check` prouve la chaîne complète, et `relay()` a été coupée en deux pour qu'elle soit vérifiable — une fonction qui avale ses propres erreurs ne peut être prouvée par rien — détail ci-dessous |
| **M4septvicies — ce que GitOps a changé** | ✅ **soldé, décision d'identité prise dans la couche qui la porte** — D28 attendait trois arbitrages avant du code. L'acteur : `{"kind":"configuration"}` élargi **dans le port d'audit seul**, jamais un cinquième `Principal` que chaque `match` aurait dû ignorer. La granularité : une entrée par changement, ce qui a demandé de stocker la déclaration appliquée (`managed_declarations`) — le réconciliateur applique le fichier entier et ne savait donc jamais ce qui avait bougé, et les trust policies n'étaient stockées nulle part. La portée : les trois choses réconciliées, parce qu'en traiter une aurait réglé un tiers du problème en donnant l'impression du tout — détail ci-dessous |
| **M4duodetricies — le SIGHUP que §11 promettait** | ✅ **soldé, et la promesse était fausse depuis M1** — §11, §10.4bis, la liste d'endpoints de §8 et le scénario **T21** nomment tous SIGHUP ; aucun gestionnaire n'existait, et le test `critical_t21_` ne couvrait qu'une des trois clauses de T21. Le signal relit le fichier et rejoue la réconciliation ; ce qu'un reload ne peut pas adopter est **nommé** plutôt que tu ; les trust policies restent celles du démarrage, sans quoi D28 daterait un changement de droits de la mauvaise heure. `Trigger` distingue démarrage, minuterie et SIGHUP sur la console — ouvre D31 — détail ci-dessous |
| **M4undetricies — les trust policies à chaud** | ✅ **soldé — la moitié que M4duodetricies avait refusé de faire semblant de faire** — `[[trust_policy]]` était clonée dans l'échange de jeton au démarrage, donc un SIGHUP ne pouvait pas l'adopter ; l'incrément précédent avait gardé l'ensemble courant et averti, plutôt que de laisser D28 dater un changement de droits de la mauvaise heure. `LivePolicies` tient l'ensemble derrière un `RwLock` partagé, l'échange le relit à chaque exchange, et la console lit le **même** — deux copies auraient donné deux réponses à « quelle forge peut publier », dont une que personne n'applique. Vu : ajout puis retrait sur le process qui tourne, journalisés tous les deux — détail ci-dessous |
| **M4tricies — M5 par la moitié qui ne lie rien** | 🟡 **commencé, et la coquille n'est compilée par rien** — D13 tient, donc la question posée est devenue « quelle part de M5 ne lie pas GTK ». Plus grande qu'attendu : `tray` (§10bis.3ter, avec une précédence que le design ne tranchait pas) et `deeplink` (§10bis.3quater) sont purs, dans `notify-client-core`, testés partout. `web/src-tauri` est écrit et **hors du workspace**, sous la même réserve que D17 ; complété jusqu'au point où le premier build sera un vrai test — `build.rs`, `capabilities/`, et les cinq icônes que le code nommait déjà, dérivées de sources SVG par `task gen` — détail ci-dessous |
| **M4untricies — « zéro artefact navigateur »** | ✅ **la couche native de §10bis.3bis, dans le front partagé — donc testable et regardable ici** — un drapeau runtime (`lib/desktop.ts`) pose `is-desktop` sur la racine avant le premier paint, et la feuille de style y accroche la sélection, les curseurs, la typo système, les scrollbars et l'anneau de focus. Le corps markdown est exempté (`data-selectable`) : c'est du contenu qu'on copie. Et le `eval` d'un `pushState` de la coquille est remplacé par un événement que le front écoute — l'ancien passait **à côté du garde de route** qui fait survivre un permalien ouvert déconnecté — détail ci-dessous |
| **M4duodetricies bis — les presets DND, une seule fois** | ✅ **le calcul que trois menus allaient dupliquer** — §10bis.3ter met `1h`, `jusqu'à demain 8h` et `désactiver` dans le menu tray, §9.2 les met dans le mode `:`, et chacun doit transformer un libellé en **instant** avant d'écrire la préférence. `notify_client_core::DndPreset` le fait une fois, avec la zone de la *préférence* et non celle de la machine (§7.4). « Demain 8h » résout au **prochain** 8h — à 01:00 le sens littéral silencerait trente et une heures. Le CLI gagne `dnd on --until-morning`, donc le calcul a un appelant aujourd'hui — détail ci-dessous |
| **M5 — Tauri desktop** | 🟡 **la coquille compile enfin, et le tray est vivant — mais §10bis.3 a huit puces et cette moitié n'en tient pas la moitié** — D13 soldée (`che-mise-webkit`), premier build jamais obtenu de `web/src-tauri` : un `TrayIconBuilder` réel avec menu (statut, DND rapide, ouvrir l'inbox, quitter), état lu depuis `Session`/`Store`/l'élection de flux (§9.3bis) sur le même montage que le TUI, fermeture qui réduit dans le tray. **Poll et non poussé** — `notify-client-core` n'a toujours aucun consommateur SSE, donc ceci reste le même appel que `notify ls`, automatisé, pas le « temps réel » que §10bis.3 promet, et c'est sans doute le manque qui pèse le plus sur la suite. Restent non construits : quick panel (§10bis.3quater), updater signé, badge dock/taskbar (§10bis.3bis), détection tray Linux au lancement, raccourci global, CLI sidecar — tous nommés dans `web/src-tauri/README.md` plutôt que découverts. `notify_client_core::tray` et `::deeplink` restent la moitié pure et testée partout — détail ci-dessous |
| **M6 — Tauri Android** | 🟡 **commencé par la moitié serveur, pour la même raison que M5 par la sienne** — `notify-push-unifiedpush` cesse d'être un fichier d'un commentaire : la boîte scellée de §6.2 (X25519 éphémère, HKDF-SHA256, `XChaCha20-Poly1305`) et le transport qui l'utilise pour livrer, câblé dans `push_transports()` comme Web Push l'est depuis M3.5 — `[push.unifiedpush]` quitte la liste de `Config::unhonoured()`. Testé partout, jamais contre un vrai distributeur : rien ici n'en tient un. Ce que §7.1bis promet et que cet incrément ne pose pas : l'endpoint push **entrant** que le serveur est censé exposer lui-même — ouvre D32. L'enrôlement X25519 (Android Keystore) et le distributeur embarqué restent du Kotlin, hors de ce workspace, et rien ne peut encore l'y construire — l'image de dev ne porte pas le SDK/NDK Android, D33 — détail ci-dessous |
| **M6bis — la vignette de row** | ✅ **soldée, et vue** — D30, ouverte en M4quatervicies — la troisième surface de §10.3 câblée : `notify-avatars` gagne un second décodeur (§15 élargi, la même décision que M6 sur la première moitié), `notify_app::Fanout::derive_thumbnail` la déclenche après chaque publish/revise qui montre un `Image::Attachment`, et `GET /attachments/{id}/thumbnail` la sert — par l'API et non présignée, pour la raison que l'avatar donne déjà. Vérifiée contre un vrai Postgres/RustFS et un vrai Chrome : `notify publish --image` puis une row qui rend un `<img>` 128×128 chargé depuis la nouvelle route. Solde D30 — détail ci-dessous |
| **M6ter — le plafond par clé** | ✅ **soldée — D24, la moitié de phrase de §8.2 restée en blanc depuis M4octodecies** — la décision qu'elle réclamait (« qui a le droit de fixer le plafond d'une clé ? ») est prise : platform-admin seulement, jamais le porteur de la clé lui-même. `ApiKey::publish_per_minute`, `PATCH /apikeys/{id}/limit`, et `Writer::check_quotas` qui lit le plafond sur le `Grant` plutôt que de rouvrir `PublishNotification::new` — `Fanout` a déjà montré la manœuvre pour D30. Vérifiée contre un vrai Postgres (la colonne `BIGINT` qui évite au `u32` du domaine de se faire jamais refuser sa propre valeur) et par l'API (admin accepté, porteur refusé). Solde D24 — détail ci-dessous |
| **M7 — Tauri iOS** | 🟡 **commencé par la moitié serveur, pour la même raison que M6 par la sienne** — `notify-push-apns` cesse d'être un fichier d'un commentaire : authentification par jeton `.p8` (ES256, JWT signé par `jsonwebtoken`, comme la VAPID de M3.5), boîte scellée de §6.2 — désormais `notify-sealed-box`, son propre crate depuis cet incrément, `notify-push-unifiedpush` en étant le premier appelant plutôt que le seul — et le transport câblé dans `push_transports()`, `[push.apns]` quittant `Config::unhonoured()`. Testé partout, jamais contre `api.push.apple.com` : rien ici n'en tient un. Le payload porte `mutable-content` et une alerte-placeholder non vide, pas `content-available` — c'est ce qui fait tourner l'extension Swift *avant* affichage, vérifié contre la documentation d'Apple plutôt que supposé. Le Swift — Notification Service Extension, enrôlement Keychain, tout le build iOS — reste hors de ce workspace, et cette fois **aucun correctif amont n'existe** : contrairement à D33, aucune image Linux ne fera jamais tourner Xcode — D34 — détail ci-dessous |

**Découpage de M1** :

| # | Livrable | État |
|---|---|---|
| M1.1 | `notify-domain` — entités, invariants, machine à états | ✅ commité (`240ff36`) |
| M1.2 | `notify-ports` — traits async + doubles in-memory | ✅ commité (`9cd9414`) |
| M1.3 | `notify-app` — 12 use cases, `Caller`/`Grant` | ✅ écrit |
| M1.4 | `notify-oidc` — découverte, JWKS + rotation, Argon2id | ✅ écrit |
| M1.5 | `notify-http-actix` — 22 routes REST, `problem+json`, OpenAPI | ✅ écrit |
| M1.6 | SSE + WebSocket, `Last-Event-ID` | ✅ écrit |
| M1.7 | `notify-persistence-pg` + `notify-eventbus-pg` | ✅ écrit |
| M1.8 | Composition root `notify-server` | ✅ écrit — voir ci-dessous |
| M1.9 | Spec OpenAPI + client Rust généré + `notify publish`/`update` | ✅ écrit |
| M1.10 | Web v0 — shell shadcn-vue, login PKCE, inbox SSE, panneau détail | ✅ écrit — voir ci-dessous |
| — | `crates/oidc-mock` — provider OIDC de dev (ADR-0014) | ✅ écrit |
| — | Accès par groupe OIDC (§14.2) | ✅ écrit |

**M1.8 en détail.** Configuration figment en couches (schéma §11 complet, validée au démarrage — un backend non implémenté est **refusé** et non rétrogradé en silence, et `Config::unhonoured` liste ce que ce build parse sans l'honorer) ; câblage des adapters et des use cases ; réconciliation déclarative ; boucle de purge leader-only ; `/healthz` et `/readyz` réels ; arrêt gracieux ; UI Scalar sur `/api/docs`.

Vérifié en le lançant : login OIDC → accès dérivé du groupe → publish → inbox → SSE avec deux révisions d'un même `external_id`, contre un vrai Postgres. **Deux bugs que seul le fait de le lancer pouvait révéler** :

- **Une connexion Postgres par client SSE.** `EventBus::subscribe` est appelé par *connexion*, et §8.2 autorise 20 flux par user — cent personnes avec un navigateur et un téléphone épuisaient le pool de l'instance, et la panne ressemblait à « la base refuse des connexions » plutôt qu'à « le transport de flux les retient ». Corrigé par un `SharedBus` dans la composition root : un `LISTEN` par replica, rediffusé en process. C'est d'ailleurs ce que §11 dit déjà — le poke est *par replica*.
- **`SIGTERM` finissait en abort.** Un connecteur sqlx détruit après la fin du runtime Tokio panique dans son destructeur, et une panique dans un destructeur abort au lieu de dérouler. La séquence d'arrêt draine donc les tâches et ferme le pool **pendant que le runtime vit encore**.

Deux limites assumées, écrites là où elles se voient :

- un binding `[[channels.managed.access]]` par `email` ou `oidc_sub` ne devient un membership qu'une fois cette personne connectée au moins une fois (un membership exige un `UserId`, créé au premier login). Le réconciliateur le **signale** et tourne aussi sur timer, donc l'accès arrive dans la minute qui suit ce premier login plutôt qu'au prochain redémarrage. Les bindings par groupe n'ont pas ce problème : ils se résolvent depuis le token ;
- `reload = "sighup"` (§11) n'est pas câblé sur le signal ; le timer couvre le besoin en attendant.

**M1.10 en détail.** `web/` : Vue 3 + Vite + Tailwind v4, composants shadcn-vue vendorés (ADR-0004), tokens sémantiques dans un seul fichier (§10.2 — aucune couleur brute ailleurs, c'est la règle bloquante de §10.6), client TS généré par HeyApi depuis la spec commitée (ADR-0009). Login Authorization Code + PKCE porté par la SPA (§5.1, ADR-0016), shell 3 zones (§10.4), inbox groupée par `group_key` alimentée par SSE (ADR-0015), panneau détail avec timeline des révisions, actions (§2.3) et receipts, permaliens `/n/<id>` et `/c/<id>`.

Vérifié en le lançant, contre un vrai Postgres et le provider de dev : découverte OIDC → authorize → token (PKCE S256, refresh + `groups`) → préflight CORS → canaux → publish → flux SSE live. **Trois bugs que seul le fait de câbler un client pouvait révéler**, tous les trois côté serveur :

- **Le flux ne livrait rien en direct.** `SharedBus::subscribe` transformait le récepteur `broadcast` en flux avec un `poll_fn` qui reconstruisait `recv()` à chaque poll et le jetait ensuite — or ce futur *désenregistre son waker en étant droppé*. Aucun poke ne réveillait plus personne : la connexion restait ouverte, le keep-alive coulait sur son propre timer, le backlog se rejouait correctement à la connexion, et seuls les événements *live* n'arrivaient jamais. Le test unitaire passait avec une deadline de 2 s, c'est-à-dire sur le réveil du timer de `timeout` et non sur le poke ; il est passé à 200 ms, ce qui le rend incapable de repasser par accident.
- **`Event` sérialisait deux fois la même clé.** L'audience et le payload sont tous deux `flatten` — c'est ce qui donne la forme plate que §8.1 documente — mais `Audience::Channel { id }` et le `id` d'un `receipt` se disputaient le même nom, comme `recorded_at` entre l'enveloppe et un `interaction`. serde émettait la clé deux fois et le document ne se relisait plus : `GET /sync` et le WebSocket rendaient des événements `receipt`/`interaction` qu'aucun client ne pouvait désérialiser. L'audience s'écrit désormais `channel_id`/`user_id` et l'horodatage d'enveloppe `logged_at` ; deux tests verrouillent ces noms, et le doc de `Event` dit lesquels sont réservés.
- **La trame SSE ne disait pas de quel canal elle venait.** Le `data:` ne portait que `event.payload`, alors que §8.1 montre `channel_id` dans la trame. Un client qui n'a jamais vu une notification ne pouvait pas la classer sans un `GET /notifications/{id}` par arrivée — exactement le suivi que `RevisionEvent` est dénormalisé pour éviter. Le `data:` est maintenant l'événement entier, ce qui rend SSE et WebSocket rigoureusement identiques comme §8.1 le promet.

En marge : `task adr` numérotait en octal (`$(( 0014 + 1 ))` = 13), donc l'ADR suivant serait entré en collision avec un existant — et se serait cassé net à `0008`/`0009`, qui ne sont pas de l'octal valide.

Trois limites assumées, écrites là où elles se voient :

- le refresh token vit en `sessionStorage` jusqu'au service worker de M3 (ADR-0016) — l'access token, lui, ne quitte jamais la mémoire ;
- le badge « reçue en retard » (§7.2) ne couvre que les arrivées rejouées après reconnexion : la version complète a besoin du curseur de sync par device, qui est M2 ;
- la palette `Cmd+K` (§10.4) n'est pas là. Elle a besoin de `GET /search` **et** des écrans vers lesquels elle switche (Canaux, Devices, Admin), qui sont M2 — la livrer maintenant reviendrait à chercher dans un seul écran.

**Fermer M1 — ce que les portes ont révélé.** Les dix incréments étaient écrits ; passer les portes que le projet s'est lui-même données a sorti quatre choses, toutes hors du code produit et toutes du même genre : des garde-fous qui ne gardaient rien.

- **`task audit` était rouge, et le serait resté.** RUSTSEC-2023-0071 (`rsa`, attaque Marvin, sans correctif amont) arrive par `jsonwebtoken`. L'attaque vise les opérations à **clé privée** RSA ; ce produit n'en fait aucune — `notify-oidc` ne fait que *vérifier* des RS256 avec les clés publiques du JWKS (§5.1), le mock signe en ES256, et les jetons fédérés sont Ed25519 (§5.3). L'alternative (backend `aws_lc_rs`) imposerait une toolchain C à toutes les cibles, y compris les cross-compilations Tauri de M5-M7. L'exception est donc documentée dans `.cargo/audit.toml` et `deny.toml`, avec sa condition de réexamen : le jour où une clé privée RSA entre dans ce workspace.
- **`cargo deny` refusait nos propres crates.** `wildcards = "deny"` vise une dépendance crates.io en `*` ; il frappait les onze crates internes, déclarées par `path` donc sans version. `allow-wildcard-paths` ne s'applique qu'aux crates non publiables — elles ne l'étaient pas déclarées. Elles le sont maintenant (`publish = false` hérité du workspace), ce qui est vrai en soi : rien ici ne part sur crates.io, et publier `notify-client-api` un jour sera une ligne délibérée dans son propre manifeste.
- **`task audit:web` avalait ses résultats.** Le `|| true` existait pour ne pas échouer tant que `web/` n'existait pas — et le jour où il a existé, il a masqué GHSA-52cp-r559-cp3m (`js-yaml`, via le générateur HeyApi). Un projet absent est sauté explicitement maintenant ; un projet présent avec une alerte fait échouer la tâche, seule raison de la lancer. L'avis est corrigé par un `override` pnpm.
- **La matrice `oidc-compat` ne testait rien** — détaillé en §15. C'est le plus grave des quatre, parce que c'est celui sur lequel §15 s'appuie pour dire « ne jamais merger un changement d'auth sans elle ».

Restait aussi la CI elle-même : les jobs `check` et `contract` n'installaient jamais les dépendances de `web/`, donc `task recu` y aurait sauté la génération du client TS **en silence** — un job `contract` vert sans avoir vérifié le contrat qu'il existe pour vérifier. Et le job `e2e` réclamait `--privileged` plus la socket Docker pour des testcontainers qui n'existent nulle part dans l'arbre.

Portes au vert à la fin de M1 : `task lint:check`, `task test` (Rust + web), `task test:critical` (72), `task test:e2e` (43, contre un vrai Postgres), `task test:oidc-compat` (leg `mock`), `task audit` (cargo + deny + pnpm + trivy), couverture domaine+app à 96,4 % pour un plancher à 90 %, et `task recu` idempotent.

**Découpage de M2** :

| # | Livrable | État |
|---|---|---|
| M2.1 | Profils users (§2.4) — champs, règle d'override OIDC, `GET/PUT /users/me/profile` | ✅ écrit |
| M2.2 | Avatars (§2.4) — adapter `attachments-s3`, décodage sûr, `PUT /users/me/avatar` | ✅ écrit |
| M2.3 | Service accounts (§5.2bis) — `GET/POST/DELETE /serviceaccounts` + leur profil | ✅ écrit |
| M2.4 | `notify-client-core` — store SQLite WAL, curseur, élection du flux (§9.3bis) | ✅ écrit — signal local compris |
| M2.5 | CLI (§9.1) — `ls`, `channel`, `device`, complétions, `--output json\|table` | ✅ écrit — `set-ttl` compris ; `dnd` sans endpoint (M3) |
| M2.6 | TUI ratatui (§9.2) — layout 3 zones, mode `:` sur le parseur clap | ✅ écrit — les commandes `:` s'exécutent |
| M2.7 | Sync offline : compaction, compteur d'expirés et badge retard (§7.2) | ✅ écrit |
| M2.8 | Web : Settings (profil, avatar), Devices, clés, service accounts, canaux (§10.4) | ✅ écrit |

L'ordre n'est pas libre : M2.8 consomme les API de M2.1–M2.3, M2.6 consomme le store de M2.4, et le badge retard de M2.7 a besoin du curseur par device que M2.4 tient. M2.2 est le premier à réclamer un adapter `AttachmentStore` de production (§14.3), donc le premier à sortir du périmètre « aucun bucket » de M1.

**M2.1 en détail.** Un `Profile` sur le `User` : `display_name`, `timezone`, `locale`, plus l'ensemble des champs que la personne a édités elle-même.

Cet ensemble **est** la fonctionnalité. §2.4 dit « un champ modifié localement n'est plus écrasé par les syncs OIDC suivants », et sans mémoire de ce qui a été édité il n'y a que deux comportements possibles, tous les deux faux : ou l'IdP écrase le choix de la personne à chaque login, ou il ne met plus jamais rien à jour. Le domaine enregistre donc le champ, pas la valeur — « ce `display_name` est le sien » — et `PUT` avec `null` **rend** le champ à l'IdP plutôt que de le vider, ce qui est la seule façon de revenir en arrière sans support.

Le piège est côté sérialisation, et il annule la fonctionnalité en silence : serde replie « absent » et « `null` explicite » sur la même valeur — `#[serde(default)]` produit `None` pour l'un, et `null` se désérialise en `None` pour l'autre. Rendre un champ à l'IdP devenait alors indistinguable de « ne touche à rien », c'est-à-dire l'inverse de ce que la requête demande. Le corps porte donc un `deserialize_with` qui n'est appelé que si la clé est présente, et le domaine dit les trois états par un enum (`FieldEdit::Follow` / `Set`) plutôt que par un `Option<Option<String>>` que personne ne relit correctement.

Deux choses trouvées en câblant ça :

- **Le login écrivait en base à chaque requête.** `Authenticate::bearer` rafraîchissait les claims puis appelait `upsert` inconditionnellement — et l'auth par bearer se rejoue à *chaque* appel (§5.1). C'est exactement l'écriture par requête que §14.2 refuse pour justifier le modèle dérivé des groupes, sur le chemin le plus chaud de l'API. La synchronisation renvoie maintenant si quelque chose a changé, et n'écrit que dans ce cas.
- **Le fuseau est validé contre la base IANA**, pas contre une regex. Il sert de défaut aux quiet hours (§7.4, M3) : un `Europe/Pris` accepté aujourd'hui devient une notification qui ne part pas dans six mois, et le lien entre les deux serait introuvable.

**M2.2 en détail.** Trois choses, et l'ordre entre elles est la sécurité.

`notify-avatars` est **la seule surface de décodage média du serveur** (§15), donc elle est petite, sa liste de décodeurs est explicite — PNG, JPEG, WebP compilés, rien d'autre : un format qu'on n'accepte pas ne peut pas avoir de faille qui nous concerne — et l'ordre des vérifications n'est pas un détail d'implémentation : taille avant tout parsing ; format d'après les **octets** et non d'après le `Content-Type`, qui est une déclaration ; dimensions lues dans l'**en-tête** avant décodage complet, parce que 30 000 × 30 000 pixels font quelques kilo-octets de PNG et 3,6 Go de RGBA décodé ; puis décodage, recadrage carré et **ré-encodage**. C'est le ré-encodage qui strippe les métadonnées : « on a retiré les tags qu'on connaît » est une promesse plus faible que « on a écrit un fichier neuf depuis les pixels ».

`attachments-s3` implémente enfin le port (§14.3) — opérations S3 standard uniquement, path-style, presign court — et sa conformité tourne contre un vrai RustFS, pas contre un mock : « HEAD sur une clé absente répond 404 » est une affirmation sur l'implémentation de quelqu'un d'autre.

L'avatar est le seul upload qui **transite par le serveur** (§2.4), donc le port a gagné `put`/`get` à côté des presign. Il est servi par `GET /avatars/{id}` plutôt que par une URL signée : un avatar s'affiche à côté de chaque révision publiée par son propriétaire, donc une signature par rendu serait un aller-retour de signature par ligne d'inbox. Et par id d'objet, jamais par id d'utilisateur — un `/users/{id}/avatar` serait exactement la lecture de profil d'autrui que §2.4 refuse.

Deux décisions écrites là où elles se voient :

- **l'avatar n'est jamais synchronisé depuis l'IdP.** OIDC publie une claim `picture`, et la suivre voudrait dire que le serveur va chercher une URL arbitraire trouvée dans un jeton — une primitive de SSRF pointée sur tout ce que le cluster peut joindre. Un avatar arrive par upload ou pas du tout ;
- **l'ancien objet n'est supprimé qu'après que le nouveau est stocké et référencé.** L'ordre inverse ouvre une fenêtre où le profil pointe sur des octets disparus, et un échec au milieu la rend définitive ; celui-ci laisse au pire un orphelin, que la purge sait retrouver.

**M2.3 en détail.** `GET/POST/DELETE /serviceaccounts`. La seule vraie question était **qui administre un bot**, et §5.2bis ne la tranchait pas.

La réponse suit deux précédents déjà dans le code plutôt que d'inventer un modèle :

- un compte **managed** refuse l'API à tout le monde, exactement comme `DELETE /channels/{id}` refuse un canal managé (§11) — laisser un appel API gagner ne ferait que déplacer le problème à la prochaine réconciliation, qui l'annulerait ;
- un compte **dynamic** appartient à son créateur, comme `CreateChannel` fait déjà du créateur le gestionnaire du canal, et pour la même raison écrite là-bas : un objet que personne ne peut administrer se répare en base.

D'où un `created_by` sur le compte, `NULL` pour les managés — la config les administre, et celui qui l'a appliquée n'en est pas propriétaire.

Ce que ça laisse ouvert est exactement la tension que §5.2bis nomme : **un bot dont le créateur est parti continue de tourner** — ses clés sont les siennes, c'est tout l'intérêt — mais plus personne ne peut l'éteindre. GitOps est la réponse pour tout ce qui est durable, et le rôle plateforme (§5.4, M4) ferme le cas des comptes déjà créés. Distribuer la désactivation à tout le monde aurait été pire que le trou.

Trois refus valent d'être notés : un **service account ne peut pas créer de service account** (une escalade sans trace exploitable — la chaîne du « qui a demandé ça » doit finir sur quelqu'un à qui on peut poser la question) ; un **nom déjà pris est refusé, jamais adopté** (le nom *est* l'identité ici, et le renvoyer laisserait quelqu'un s'approprier le bot d'un autre) ; et **désactiver un compte inexistant réussit** plutôt que de dire lequel existe. La désactivation révoque les clés **dans le même appel** (T25) : une clé encore valide est un identifiant qui dort dans la CI de quelqu'un.

**M2.4 en détail.** Un store SQLite par profil, le curseur et l'identité device dedans, et une élection par lock consultatif à côté.

**SQLite parce que la contrainte est multi-*process*.** §9.3bis veut que plusieurs frontales lisent pendant qu'une écrit ; c'est ce que fait le mode WAL, et ce que les stores embarqués pur-Rust ne font pas — `redb` et ses pairs prennent un verrou exclusif sur le fichier, ce qui transformerait « le TUI est ouvert » en « la CLI ne peut pas lire ». Le coût est réel et écrit dans le module : `rusqlite` en `bundled` compile SQLite depuis du C, donc **construire un client demande un compilateur C**. Ce n'est pas le compromis que ce workspace a refusé pour le *serveur* (backend pur-Rust de jsonwebtoken, pour que l'image se construise d'un `cargo build` nu) — une CLI n'est pas une image de conteneur, et SQLite est le C le plus portable qui soit.

**L'élection est un lock, pas un drapeau.** Le détenteur du flux est celui qui tient un verrou exclusif sur un fichier à côté du store. Un process qui meurt le libère *en cessant d'exister* — crash, `kill -9`, capot rabattu. Un drapeau en table aurait demandé un bail, un battement de cœur et un chemin de reprise pour le détenteur qui n'est jamais revenu, le tout pour répondre à une question que le système d'exploitation tranche gratuitement. Acquérir ne bloque pas : la frontale qui perd lit le store et continue, ce qui fait que l'app en tray gagne naturellement en étant simplement là avant.

Deux choses trouvées en l'écrivant :

- **`libsqlite3-sys` déclare `links = "sqlite3"`**, donc une seule version peut exister dans le graphe — et sqlx en tire déjà une par la macro `migrate!` que ce workspace utilise vraiment. `rusqlite` est donc épinglé sur la version qui partage celle de sqlx ; monter l'un impose de monter l'autre, et c'est écrit à côté de la dépendance ;
- **le lock n'a demandé aucune dépendance** : `std::fs::File::try_lock` existe depuis Rust 1.89. Le MSRV passe de 1.85 à 1.89, ce qui a immédiatement fait apparaître un lint que le plancher précédent masquait (`manual_is_multiple_of` dans `notify-domain`) — un rappel utile que le MSRV n'est pas qu'une déclaration.

**Le signal local est là** (D5, soldé). La détentrice écoute sur `store.stream.sock` à côté du verrou et pousse **un octet** après chaque écriture ; les lectrices se connectent, et rechargent **depuis le store** à la réception. L'octet ne porte rien et ne doit rien porter : y mettre l'événement en ferait un second canal de livraison avec son ordre, son backlog et ses façons d'avoir tort — le store *est* le canal, la socket dit seulement « regarde à nouveau ». Trois propriétés valent d'être écrites :

- **rien n'est porteur** : socket absente, pair disparu, plateforme sans socket unix — tous les cas retombent sur le tick qui existait déjà. Une latence, jamais une panne, et c'est ce qui autorise à ne rien réessayer ni acquitter ;
- **on ne pique que si quelque chose a bougé**. `sync_into` répond ce qu'il a écrit, et une détentrice au repos ne réveille personne : une lectrice tirée du sommeil quatre fois par seconde serait pire que celle qui attend son tick ;
- **Windows n'est pas câblé** : l'équivalent est un named pipe, et là-bas le tick est tout le comportement. Écrit ici plutôt que découvert en M5.

L'accept est fait **en passant**, par l'écrivain lui-même sur un listener non bloquant, ce qui évite un thread et donc son arrêt propre.

**M2.5 en détail.** `notify ls`, `notify channel list|create|join`, `notify device list|enrol`, chacun en `--output json|table`. Les appels descendent dans `notify-client-core` et pas dans la CLI (§9.3) : la même logique servira au TUI et à la coquille Tauri, et une règle écrite ici devrait l'être trois fois.

`device enrol` écrit l'identité device **dans le store partagé** (M2.4), pas dans le process : l'enrôlement est par *profil*, donc le TUI et l'app desktop en héritent au lieu d'en créer chacun un — trois devices pour une machine feraient aussi compter « vu par 3/5 » trois fois.

`notify channel set-ttl` existe maintenant, au-dessus du `PATCH /channels/{id}` que D2 a ajouté : `--default 7d`, `--max 48h`, et `--max 0` pour retirer le plafond. Seul ce qui est passé part sur le fil — un `PATCH` qui répéterait le bouton qu'on n'a pas touché est la façon dont deux admins sur deux écrans s'écrasent.

**Une commande de §9.1 n'a toujours pas d'endpoint** et est donc absente plutôt que simulée : `notify dnd …` attend `GET/PUT /preferences/dnd` (§7.4), qui arrive en M3.

Une verrue relevée en câblant : la spec **inlinait le schéma des items** dans `Page_Channel` et `Page_Notification` au lieu de référencer `Channel` et `Notification`. Le client TS s'en moquait (typage structurel) mais progenitor y frappait un second type nominal — `PageChannelItemsItem` — pour la même chose, que `notify-client-core` isolait derrière un alias. C'était D3, soldée depuis (voir « Solder D1 et D3 » plus bas) : les alias `ChannelSummary`/`InboxEntry` n'existent plus, la CLI et le TUI nomment `Channel` et `Notification`.

**M2.6 à M2.8 en détail.** Le TUI est en place — architecture Elm-like (un `App`, un enum `Message`, un `update` seul autorisé à muter, un `draw` sans effet de bord), layout 3 zones miroir du web, hook panic qui restaure le terminal **avant** de crasher, et chaque chaîne venue du serveur strippée de ses séquences ANSI avant rendu (§2.3 — le serveur strippe déjà à l'ingestion, mais ici le terminal *est* le moteur de rendu). Le mode `:` passe la ligne au **même parseur clap** que le shell, ce qui est le mécanisme de parité de §9 : une sous-commande ajoutée à la CLI y est disponible sans une ligne de code TUI.

Le sync client remplit le store depuis `GET /sync` en suivant le curseur, et remonte les deux compteurs que §7.2/§7.2bis rendent visibles : les révisions repliées par la compaction, et les notifications expirées par canal pendant l'absence.

Côté web, Settings et Administration. Settings existe pour rendre visible ce que l'API modélise et qu'aucun autre client ne montre : **quels champs t'appartiennent**, avec un bouton pour les rendre à l'IdP. Administration met canaux, devices, clés et service accounts sur un écran parce que c'est une seule question — *qu'est-ce qui peut agir en mon nom, et sous quelles règles* — et le formulaire de clé n'offre **que** des propriétaires service account, ce qui est §5.2 transformé en interface plutôt qu'en documentation.

L'écran des canaux ne décide pas qui a le droit d'éditer une policy : §5.1 dérive l'autorisation par requête, donc l'interface propose et affiche le refus s'il vient. Ce qu'elle cache est ce que la **donnée** tranche : un canal `managed` n'a aucun contrôle du tout, parce que la config le possède (§11) et que la prochaine réconciliation annulerait l'appel.

**Trois trouvailles, dont une grave :**

- **`task recu` était cassé et ma vérification ne le voyait pas.** L'upload d'avatar de M2.2 déclarait son corps en `Vec<u8>`, qu'utoipa rend en tableau d'entiers et que progenitor refuse pour `application/octet-stream`. La génération du client Rust échouait donc — et mon contrôle d'idempotence (« les fichiers n'ont pas changé ») **passait justement parce que rien ne tournait**. Exactement le mode d'échec dénoncé plus haut dans ce document, reproduit par moi. Le corps se déclare maintenant `string`/`format: binary`, et la règle qui manquait est écrite en D8 (§14.4) : une vérification doit prouver que le travail a eu lieu avant de vérifier son résultat.
- **Le client Rust ne pouvait pas consommer `GET /sync`.** progenitor rendait `Event` — un `allOf` de deux `oneOf`, ce qui produit la forme plate de §8.1 — en `pub enum EventVariant0 {}`, un type **inhabité** : la désérialisation ne marchait que tant qu'il n'y avait aucun événement. `notify-client-core` lisait donc la réponse avec son propre modèle minimal, exception documentée dans le module. C'était D1, soldée depuis.
- **La spec inlinait les items paginés** au lieu de référencer `Channel`/`Notification`, ce qui faisait frapper à progenitor un second type nominal pour la même chose (M2.5). C'était D3, soldée depuis.

**Fermer M2 — les quatre manques soldés.** Le signal local de §9.3bis, l'exécution des commandes `:`, le badge retard et l'admin canaux étaient nommés ici plutôt que passés sous silence ; ils sont faits, et §14.4 dit lesquels par leur numéro (D2, D4, D5, D6). Deux choses n'étaient pas au programme et sont sorties en les faisant :

- **Un refus « managed » rendait 422 alors que §8 documentait 409 sur les trois routes concernées.** `Origin::ensure_mutable_through_api` refusait avec un `Malformed`, donc le code stable sur le fil était `field.malformed` : un client qui branche dessus aurait traité « va changer la config » comme « tu as mal tapé ». La requête n'a rien de malformé — elle est comprise, et refusée par une question de propriété. D'où un variant à part, `origin.managed`, de catégorie `Conflict`. Le document ne mentait pas ; c'est le code qui ne le tenait pas.
- **Le store local ne gardait que le *contenu* d'une révision**, pas l'événement. Une lectrice ne pouvait donc rien rendre depuis le cache — ni statut, ni état, ni horodatage —, ce qui rendait le signal local inutilisable pour ce à quoi §9.3bis le destine. Le cache stocke maintenant l'événement **verbatim** (ADR-0013 : la forme sur le fil *est* le contrat) et en relit ce qu'il dessine, donc un build ultérieur peut afficher ce que celui-ci a mis de côté.

Deux formes valent d'être notées parce qu'elles se ressemblent et se décident autrement :

- **le plafond de TTL se retire en écrivant `"0"`, pas `null`.** §7.2bis donne déjà à `"0"` le sens « illimité » partout où une durée apparaît, et un plafond *à jamais* **est** l'absence de plafond : une seule orthographe. Le tri-état de §2.4 (absent ≠ `null`) aurait été le réflexe, et il aurait été faux ici pour une raison mécanique — progenitor rend tout champ optionnel avec `skip_serializing_if`, donc le client Rust sait omettre une clé mais jamais l'envoyer nulle. ADR-0009 promet que les clients viennent du contrat ; le contrat n'a donc pas le droit de décrire ce qu'ils ne savent pas dire. Le profil garde son `null` parce que rien de généré n'écrit encore ce corps-là — le jour où ça arrive, D-profil rejoint cette liste ;
- **`allow_forever` reste hors du `PATCH`.** §7.2bis en fait une décision d'opérateur : la laisser passer par l'API rendrait la réponse de la config consultative.

Et une règle appliquée à la présentation plutôt qu'aux règles métier : **chaque commande CLI renvoie une `String` au lieu d'imprimer**. C'est ce qui permet au mode `:` d'*exécuter* les commandes et d'afficher **la réponse du shell**, pas une seconde mise en forme de la même chose — la §9.3 (« toute logique qui apparaît dans `notify-cli` doit redescendre ») vaut aussi pour ce qu'on écrit à l'écran, parce que deux rendus divergent et que la divergence est invisible tant que personne ne compare deux écrans.

**Solder D1 et D3 — le contrat devient descriptible.** Les deux dettes que M2 a créées disaient la même chose : `ADR-0009` promet que les clients viennent du contrat, et le contrat décrivait deux formes qu'aucun générateur ne sait modéliser. Elles se soldent au même endroit — `crates/notify-ports/src/schema.rs`, les deux seuls schémas **écrits** de tout le document, chacun avec la raison de l'être en tête de module.

**D1 : `Event` est décrit comme les cinq événements qu'il est.** Un `oneOf` de cinq branches, chacune complète — son discriminant `event`, l'enveloppe (`cursor`, `logged_at`) et l'audience — au lieu de l'`allOf[oneOf, oneOf, object]` que la composition produisait. **Les octets sur le fil ne bougent pas d'un iota** ; seule leur description change, et progenitor en tire cinq variantes de structure là où il émettait un type inhabité. Trois choses décidées en l'écrivant :

- **la moitié « charge utile » de chaque branche reste un `$ref`**, pas une liste de propriétés recopiée. §14.4 prescrivait « cinq objets complets », et un `allOf[$ref, objet]` en est un — c'est d'ailleurs exactement la forme que `EventPayload` compilait déjà correctement, donc la preuve que progenitor la fusionne était dans le client généré. Le bénéfice est ce que la solution écrite coûtait : un champ ajouté à `RevisionEvent` atteint encore le document tout seul, et ce qui est écrit à la main se réduit à cinq enveloppes de six lignes ;
- **chaque branche porte un `title`.** Sans lui, progenitor nomme les variantes `Variant0`…`Variant4` et un client qui filtre dessus change de sens le jour où une branche se déplace. Avec, le client rend `Event::Revision { … }` jusqu'à `Event::Preference { … }` ;
- **l'audience est fixée par genre** : `preference` porte `user_id`, tous les autres `channel_id`. C'est T26 écrit dans le document et plus seulement dans le code — un client qui lit la spec ne peut pas attendre un canal sur une préférence. `Audience` et `EventPayload` ont perdu leur dérive et, avec elle, leur place dans le document : plus rien ne les référence, et une dérive laissée là aurait laissé croire que le document suit encore ces types.

Le coût que §14.4 annonçait est réel et couvert : un test sérialise un **vrai** événement de chaque genre et vérifie les deux sens — rien de ce que la branche exige ne manque à l'événement, rien de ce que l'événement porte n'échappe à la branche.

**D3 : une page référence son item.** Un `ToSchema` manuel pour `Page<T>` où `items` est un tableau de `$ref`. Deux détails qui ne se devinent pas : un corps générique n'est pas *décrit* mais **composé** (`ComposeSchema` ; `PartialSchema` en découle par l'implémentation générale d'utoipa), et le `name()` d'un type générique est un **préfixe** auquel utoipa ajoute celui de l'élément — le nommer en entier a d'abord donné `Page_Channel_Channel`. `PageChannelItemsItem` a disparu, le document a maigri d'une copie complète de `Channel`, `Notification` et `Revision`, et les alias `ChannelSummary`/`InboxEntry` de `notify-client-core` sont supprimés.

**Ce que ça change côté client**, et la seule chose qui *ne* revient pas au client généré : `wire.rs` n'existe plus, `sync.rs` et `store.rs` lisent l'`Event` généré. L'enveloppe de page, elle, continue d'être lue à la main, et c'est délibéré — le corps est pris en JSON pour que l'événement atteigne le store **verbatim** (ADR-0013, propriété acquise en fermant M2). Re-sérialiser une structure progenitor mettrait en cache l'événement d'un serveur plus récent *amputé de tout ce qu'il a de nouveau*, et le cache est précisément ce que rend une frontale qui a perdu l'élection (§9.3bis). Stocker les octets puis relire une copie coûte une structure de quatre champs et garde les deux propriétés.

Deux conséquences de la rigueur du type généré, notées parce qu'elles ne se voyaient pas avec un modèle tolérant :

- **le curseur se lit sur le document, pas sur l'événement analysé.** Un genre d'événement que ce build ne connaît pas ne doit pas empêcher le curseur d'avancer, sinon un vieux client cale pour de bon devant un serveur plus récent. Avec l'ancien modèle — tous les champs optionnels — tout se désérialisait, donc le cas n'existait pas ;
- **une fixture de test n'était pas un événement entier.** Celle du store omettait `audience`, `logged_at`, `priority` et `author` ; le modèle tolérant l'acceptait. Elle vérifiait donc la tolérance du parseur plutôt que le comportement du store.

Portes au vert à la fin de M2 : `task lint:check`, `task test` (482 Rust + 52 web), `task test:critical` (73), `task test:e2e` (47, contre un vrai Postgres — vérifié en comptant zéro « skipping » et un temps d'exécution qui n'est pas celui d'une suite qui se saute), `task test:oidc-compat` (leg `mock`), `task audit` (cargo + deny + pnpm + trivy), couverture domaine+app à 96,2 % pour un plancher à 90 %, et `task recu` idempotent — l'idempotence contrôlée **après** avoir vérifié que la génération avait bien tourné, ce que D8 exige et ce que la fin de M2 avait raté.

**Découpage de M3** :

| # | Livrable | État |
|---|---|---|
| M3.1 | DND dans le domaine (§7.4) — quiet hours à timezone explicite, DND ponctuel, portée globale/canal + exceptions, bypass `urgent` opt-in | ✅ écrit |
| M3.2 | `GET/PUT /preferences/dnd` (§8), le DND porté par l'événement `preference` (§8.1), et `notify dnd` — la commande de §9.1 qui attendait son endpoint | ✅ écrit |
| M3.3 | Suppression des push pendant une période DND, `deferred_by_dnd` au sync (§7.2, T10 et T19) | ✅ écrit |
| M3.4 | Digest de sortie — **un** push, pas une rafale ; job leader-only (§11) | ✅ écrit |
| M3.5 | `push-webpush` — RFC 8030/8291 (VAPID), premier adapter de production de `PushDispatcher` (§14.3), config `[push.webpush]` | ✅ écrit — câblé dans la composition root, plus `GET /push/webpush` |
| M3.6 | Web : PWA installable + service worker, abonnement Web Push, enrôlement device, DND respecté localement | ✅ écrit — ADR-0017 |
| M3.7 | Receipts : « vu par 3/5 » en `HoverCard` (§7.3, §10.3), compté en users | ✅ écrit — plus `GET /notifications/{id}/receipts` |

L'ordre n'est pas libre : M3.3 et M3.4 consomment la préférence de M3.1–M3.2, et M3.6 consomme le transport de M3.5. M3.5 est le premier à sortir du « registre vide » que §14.3 décrit comme le comportement correct de M1.

**Ce que M3 ne prend pas.** La palette `Cmd+K` (§10.4) attendait `GET /search` **et** les écrans de M2 ; elle est débloquée depuis, mais elle n'est pas dans la ligne M3 de §14 et l'y glisser diluerait un milestone déjà large. Elle reste nommée ici plutôt que passée sous silence, comme les quatre manques de M2 l'ont été.

**M3.1 en détail.** Une préférence par user, entièrement côté serveur — c'est ce qui la rend vraie sur tous les appareils à la fois (§7.4) — et lisible sans I/O : `is_muted(canal, priorité, instant)` répond par oui ou non, et rien d'autre dans le produit n'a le droit de se demander ce que « DND » veut dire.

Quatre décisions, dont trois que §7.4 impose et une qu'il ne tranchait pas :

- **la timezone est celle de la préférence, jamais du device.** `22:00–08:00, Europe/Paris` veut dire les nuits de la personne, pas celles du fuseau où son téléphone se trouve ce mois-ci. Elle est validée contre la base IANA comme le fuseau de profil de M2.1, et pour la même raison : un `Europe/Pris` accepté aujourd'hui est une notification qui ne part pas dans six mois, et le lien entre les deux serait introuvable ;
- **une plage qui passe minuit est une plage**, pas deux. `22:00–08:00` couvre la fin d'un jour et le début du suivant, et le jour de la semaine se juge sur le **début** — un vendredi 22:00–08:00 couvre la nuit de vendredi à samedi, ce que « vendredi » veut dire pour un humain et pas ce que `samedi 02:00 ∈ vendredi` dirait ;
- **`urgent` ne passe que si le destinataire l'a demandé** (défaut : non). C'est §2.3 et §7.4 dans le même sens : le publisher choisit l'insistance, le destinataire choisit ce qui a le droit de le réveiller ;
- **une exception de canal bat tout le reste**, y compris un DND ponctuel posé à la main. §7.4 ne le disait pas — il donne le cas d'astreinte (« tout couper *sauf* le canal critique ») sans dire qui gagne quand les deux se contredisent. Faire gagner l'exception est la seule lecture qui rende le cas d'astreinte utilisable : l'inverse obligerait à défaire son exception chaque fois qu'on coupe le reste, c'est-à-dire précisément au moment où on ne veut pas y penser.

**M3.2 en détail.** `GET/PUT /preferences/dnd`, un port `PreferenceRepository`, et la commande `notify dnd` que M2.5 avait laissée absente plutôt que simulée.

Trois choses décidées en le câblant :

- **le sujet n'est pas une entrée.** Aucune route ici ne prend d'id de user — il n'y a pas de chemin qui *pourrait* parler de quelqu'un d'autre. Le corps est un `DndDraft`, le couple draft/entité que ce code utilise déjà partout (`ContentDraft`, `RevisionDraft`), et il n'a pas de champ pour le sujet du tout. C'est ce qui fait de T26 une propriété de l'espace d'URL plutôt qu'une vérification qu'il faut penser à écrire dans chaque handler, exactement comme `/users/me/profile` de §2.4 ;
- **le `PUT` remplace en entier.** Les quiet hours sont une liste, et fusionner une liste n'a pas de sens univoque : un corps qui omet une plage ne se distingue pas d'un corps qui demande de la retirer. Le tri-état de M2.1 (absent ≠ `null`) répondait à une autre question — un champ scalaire dont on veut rendre la main à l'IdP — et l'appliquer ici aurait produit une API que personne ne sait écrire correctement. La CLI relit donc avant d'écrire, ce qui est aussi ce qui empêche `dnd on` d'effacer un planning au passage ;
- **`PreferenceRepository` est un port à part**, et pas une troisième méthode sur `InteractionRepository`. Un silence se pose depuis un bouton d'une notification et vit à côté des interactions qui arrivent avec ; un DND se pose dans Settings et survit à toutes les notifications de l'instance. Les deux ne se rencontrent que sur le fil.

Et c'est cette rencontre qui a produit la seule vraie contrainte : **l'événement `preference` porte les deux moitiés, quelle que soit celle qui a changé.** §8.1 le promet, un client qui décide s'il doit lever un toast a besoin des deux, et un événement qui ne porterait que la moitié écrite laisserait l'autre à deviner. Les deux écrivains passent donc par **un seul constructeur** (`announce_preference`, côté adapter comme côté double) — deux sites qui le construisent chacun de leur côté, c'est la façon dont l'un des deux finit par n'expédier que ce qu'il vient d'écrire. Un test de conformance le vérifie contre un vrai Postgres : poser un *silence* doit rendre un événement qui contient encore le DND que ce write n'a pas touché.

Deux formes valent d'être notées :

- **pas de ligne pour qui n'a rien réglé.** Le port répond `Dnd::off` quand la ligne manque : « jamais ouvert Settings » et « une préférence qui ne coupe rien » sont la même réponse pour tous les appelants, et créer une ligne au premier login mettrait une écriture sur le chemin le plus chaud de l'API pour une valeur qui est déjà le défaut. Le stockage est un document jsonb, parce que la préférence se lit et s'écrit entière et que rien ne demande jamais « qui est silencieux le mardi » — relu à travers `Dnd::validated`, donc une ligne écrite par le build du mois dernier doit encore satisfaire les invariants de celui-ci ;
- **une préférence illisible en base est `Corrupt`, pas une erreur client.** C'est nous qui avons écrit la ligne (§4, deux types d'erreur exprès).

**Vérifié en le lançant**, contre un vrai Postgres et le provider de dev : login PKCE → `PUT` → `GET` → l'événement `preference` sur `GET /sync`, adressé au user et portant les deux moitiés. Et **un bug que seul le fait de le lancer pouvait révéler** : le corps de la requête était un `Dnd`, donc il portait le `user`.

Ce champ était documenté « ignoré, réécrit par le use case », ce qui semblait suffire — les tests passaient, parce que celui qui les écrit connaît l'id qu'il vient de créer. Un vrai client ne le connaît pas, et le champ n'est pas optionnel : il est **requis**. Un `curl` avec n'importe quoi d'autre qu'un ULID reçoit une erreur de désérialisation à propos d'un champ que le serveur allait écraser, et un client qui ne connaît pas son propre id ne peut simplement pas construire de corps. « Trompeur » était la bonne intuition et la mauvaise conclusion : ce n'était pas un champ mal nommé, c'était une entrée obligatoire qui n'aurait jamais dû exister. D'où `DndDraft`, et un corps vide `{}` qui est désormais un corps complet.

**Un écart avec §9.1, assumé** : le croquis y écrit `notify dnd on|off|until 9h|for 2h|schedule|status`. `until` et `for` sont ici des options de `on` et non des verbes, parce que ce sont deux orthographes d'une même action — en sous-commandes, clap devrait proposer un `notify dnd until` tout seul, sans rien pour dire à quoi il s'applique. Et **`on` sans borne est refusé** : §7.4 fait du basculement rapide une échéance, et un DND sans fin est ce qu'on pose à 3h du matin pour le redécouvrir un trimestre plus tard — le même raisonnement que le silence de groupe de §2.3, qui exige déjà une durée.

**M3.3 en détail.** Le DND devient effectif : le `Fanout` le consulte avant de pousser, et le sync dit quel retard était voulu.

**Ce qui est supprimé, et ce qui ne l'est pas.** Seul le **push** est retenu — le poke de bus part quand même, donc une frontale ouverte continue de recevoir sur son flux et l'inbox se remplit normalement (§7.4). C'est la clause de T19 qu'une implémentation rate en coupant trop, et le test l'affirme dans les deux sens : zéro push, et un poke `Channel` bien présent. Deux autres clauses suivent : un canal excepté sonne, et `urgent` reste bloqué sans accord du destinataire.

Un détail de ce genre de code : le silence et le DND répondent à deux questions différentes sur le même instant — l'un *cette notification ou ce groupe*, l'autre *ce canal, à cette priorité, maintenant* — et les deux échouent **fermé**. Ne pas pouvoir lire une préférence fait taire le push : le faire sonner ferait vibrer un téléphone dont on ne sait justement pas s'il avait demandé le silence, alors que l'inverse coûte une notification que l'inbox garde de toute façon.

**`deferred_by_dnd` est dérivé, pas enregistré.** §7.4 veut que le client n'affiche pas « reçue en retard » sur un retard qu'il a lui-même demandé. L'implémentation évidente — une ligne par push supprimé, écrite sur le chemin de fan-out et purgée ensuite — est une table qui grossit avec chaque notification différée, pour répondre à une question de badge. À la place, le sync repose la question à la préférence : *une période de calme tournait-elle sur ce canal quand la notification a été créée ?*

Le coût est réel et écrit là où il se voit : la préférence lue est l'**actuelle**, donc réécrire ses quiet hours change la réponse pour des notifications déjà dans le backlog. C'est un badge qui apparaît ou disparaît sur une ligne que plus personne ne regarde, contre l'absence d'une table qui grossit sur le chemin le plus chaud du produit.

**Et il est à côté des événements, pas dessus.** Un `Event` est adressé à un *canal* et tous ses membres en reçoivent les mêmes octets ; ceci est la préférence d'une seule personne. Le mettre sur l'événement ferait diverger le même événement selon son lecteur, ce que §8.1 promet précisément qu'il ne fait pas — le flux et le backlog portent des événements identiques. Il rejoint donc `expired` et `compacted` sur la page, les deux autres faits par-user qu'un rattrapage rapporte.

Côté client, le store local passe en schéma **3** (`ALTER TABLE`, un cache existant n'est pas jeté) et `late_by()` rend `None` quand le retard était voulu. Contrairement à `delivered_at` — que le store estampille lui-même parce que c'est un fait sur *ce* cache — le drapeau est **passé** : c'est ce que le serveur dit d'une préférence, et lui seul la connaît.

**M3.4 en détail.** La dernière clause de T19 : à la sortie d'une période de calme, **un** push, pas une rafale.

**Pourquoi c'est un job et pas un instant.** Rien ne *se produit* quand une période de calme se termine — la fin d'un planning est l'absence d'un événement, donc il faut que quelqu'un regarde. D'où un timer, et donc leader-only (§11) : chaque replica qui le lancerait enverrait son propre digest. Ce qui décide que c'est fini est `Dnd::is_active` et rien d'autre : le store rend tout le monde qui a quelque chose en attente **avec sa préférence**, et le filtre se fait dans le use case, parce qu'un store qui répondrait à cette question serait une seconde implémentation de ce que §7.4 veut dire.

**Deux compteurs, pas une table de notifications retenues.** Le digest dit « 12 notifications pendant votre absence (2 failure) », donc deux entiers y répondent exactement. Une ligne par push supprimé grossirait sur le chemin de fan-out pour être lue une fois puis jetée, et n'achèterait que la possibilité de les *lister* — qu'aucun écran ne demande, l'inbox les tenant déjà. Les `failure` ont leur colonne parce qu'elles sont le seul statut qui vaille d'être nommé sur un écran verrouillé : la différence entre « rattrape quand tu peux » et « regarde maintenant ». C'est aussi l'exception assumée au raisonnement de M3.3 — `deferred_by_dnd` est dérivé pour ne rien écrire, le tally est écrit parce qu'un push doit partir **une fois**, et une dérivation ne sait pas dire si elle a déjà été envoyée.

Trois décisions notées là où elles se voient :

- **`PushMessage` devient une somme.** Un digest ne nomme aucune notification et n'appartient à aucun canal ; le plier dans la même structure aurait donné deux champs parfois vides et un transport qui doit de toute façon savoir dans quel cas il est. Le moment est le bon : aucun transport de production n'existe encore (M3.5) ;
- **un digest n'est jamais `urgent`**, quoi qu'il contienne. Il arrive *après* une période de calme, donc rien dedans n'est une nouvelle qui ne pouvait pas attendre — et ce qui a attendu huit heures sur demande ne doit pas ensuite sonner comme une alarme ;
- **le tally est vidé même si l'envoi échoue.** Quelqu'un qui rate un résumé a une inbox ; quelqu'un qui reçoit le même résumé toutes les minutes a un bug — et c'est précisément la rafale que §7.4 veut éviter. De même, **redéfinir sa préférence vide le tally** : un digest qui compterait une période de calme qu'on vient de réécrire rapporterait quelque chose qui n'a pas eu lieu.

**M3.5 en détail.** Le premier transport de production, et le premier moment où le registre vide de §14.3 cesse d'être la bonne réponse pour un déploiement qui a une clé.

**Ce qui est pris à une bibliothèque, et laquelle.** Le chiffrement RFC 8291 vient de `web-push-native`, **sans sa feature `vapid`**, et le choix s'est décidé sur une dépendance transitive : le crate évident, `web-push`, atteint `ece`, qui atteint **OpenSSL** — la toolchain C que ce workspace a refusée en M1 pour que l'image se construise depuis un `cargo build` nu et que les cross-compilations Tauri de M5–M7 restent possibles. La feature `vapid`, elle, tirerait une **seconde** bibliothèque JWT dans un workspace qui en a déjà une ; l'assertion RFC 8292 est donc signée avec `jsonwebtoken`, celui que `notify-oidc` vérifie. Le prix est écrit dans le manifeste plutôt que découvert plus tard : `web-push-native` est bâti contre **p256 0.13** quand le workspace est en 0.14, les deux étaient déjà dans le lockfile, et une montée de version devra les déplacer ensemble.

**Le payload est un contrat avec le service worker.** Rien dans le document OpenAPI ne le décrit — ce n'est pas une réponse HTTP —, donc `Envelope` est l'endroit où ce contrat vit, et son `kind` est le mot qu'emploie déjà `PushMessage` : un client qui sait lire le flux le lit avec le même `switch`. Le changer casse le fil avec tous les clients installés, exactement comme changer §8.1.

**Câbler le transport a sorti deux choses que le crate seul ne montrait pas** :

- **Le plafond de payload était une constante choisie quand aucun transport n'existait.** `notify-app` décidait entre charge utile et poke sur un `PUSH_PAYLOAD_HINT = 3000` écrit à la main, alors que `TransportRegistry::max_payload_bytes` — qui existait déjà — n'était appelé par personne. C'était un garde-fou qui ne gardait rien, et le genre exact que §14 dénonce ailleurs. Le budget descend maintenant de la composition root dans le `Fanout` : Web Push annonce ses 4 000 octets (RFC 8291 moins le cadre `aes128gcm`), et un déploiement **sans** transport annonce zéro — ce qui n'est pas un budget dégénéré mais la bonne réponse, puisque sans transport aucune charge utile ne passe et que sérialiser un `Content` pour le tendre à un dispatcher qui va refuser est du travail que personne n'a demandé.
- **`§8 n'avait pas d'endpoint pour la clé publique.`** RFC 8292 en fait un argument de `pushManager.subscribe()` : sans elle une PWA n'obtient **aucune** subscription, donc M3.6 était bloqué par un trou entre le transport et la configuration. L'alternative était un argument de build dans `web/`, et elle est pire là où ça compte — une rotation de clé VAPID demanderait un frontend reconstruit pour que les push continuent d'arriver, et ADR-0009 (« un client se dérive du contrat ») serait enfreint par la seule valeur qui décide si un push part. D'où `GET /api/v1/push/webpush`, ajouté à §8 ci-dessus. Il répond **501** quand le déploiement ne fait pas de Web Push : c'est `PortError::NotConfigured` épelé par la même table que toutes les capacités absentes (§14.3), donc une PWA sait *sauter* l'abonnement au lieu de réessayer une clé qui ne viendra jamais.

**Et l'environnement de dev a maintenant une clé**, parce qu'un transport câblé que personne ne peut faire tourner localement n'est pas câblé. `config/dev.toml` déclare `[push.webpush]`, la clé est mintée par `task dev:setup` dans `dev/secrets/` (gitignoré), et le raisonnement complet — pourquoi `xtask` plutôt qu'`openssl`, pourquoi un refus d'écraser, pourquoi *pas* d'allowlist gitleaks — est en §12.7. Vérifié en lançant le serveur : `/readyz` rend `{"name":"webpush","healthy":true}`, et `[push.*]` a disparu de la liste des clés non honorées au démarrage.

Deux choix mineurs, notés parce qu'ils se relisent mal plus tard : la clé publique est **derrière l'authentification** alors qu'elle est publique par définition — rien n'est protégé par là, mais rien de ce qui est sous `/api/v1` ne s'ouvre sans raison (§5.1) et le seul appelant s'abonne après login ; et c'est la seule valeur de `ApiState` qui ne soit pas un use case, parce qu'il n'y a rien à décider et qu'elle n'atteint aucun port — la règle qu'elle plie existe pour empêcher un handler d'atteindre un *repository*, ce qu'un `String` ne peut pas.

**`task audit` était rouge, et le corriger a écarté une exception plutôt que d'en écrire une.** Le scanner de secrets de trivy — et le hook gitleaks — voyaient une clé privée P-256 en dur dans les tests de `push-webpush`, ce qui est exactement ce pour quoi ils existent. La réponse réflexe était une allowlist sur ce chemin, et elle est mauvaise pour une raison qui ne se voit qu'après : une exception écrite pour une clé *de test* continue de couvrir le fichier le jour où une vraie y atterrit. La clé est donc **construite** à partir d'un scalaire fixe (`notify-push-webpush/test-utils`, la convention `notify-ports/test-utils` du workspace), ce qui la rend déterministe, valide, et secrète en aucun sens qu'un scanner ou une lectrice puissent confondre. Il ne reste plus une seule clé dans l'arbre — celle du commit `731e795` reste dans l'historique, ce qui est le prix d'un commit passé sans hooks.

**Une chose trouvée en ouvrant le chantier plutôt qu'en le faisant** : le commit `731e795` ne compilait pas. Huit lignes d'une édition inachevée étaient collées **avant** l'en-tête de `crates/notify-server/src/config.rs`, et le `PathBuf` de `vapid_key_file` n'avait pas d'import. Les hooks de pre-commit (`task recu`, `task lint`) auraient arrêté les deux ; ils n'ont donc pas tourné. C'est le mode d'échec de D8 sous un autre angle — là, une vérification passait sans travail ; ici, le travail est passé sans vérification. La leçon est déjà écrite en §14.4 et n'a pas besoin d'une dette de plus : elle a besoin que les hooks soient installés (`task init`), et le fait qu'un commit ait pu atterrir sans eux est ce qui vaut d'être noté.

Portes au vert à la fin de M3.5 : `task lint:check`, `task test` (602 Rust + 52 web), `task test:critical` (85), `task test:e2e` (54, contre un vrai Postgres — 54 exécutés, zéro sauté, ce que le compte prouve), `task test:oidc-compat` (leg `mock`, 3 exécutés et 0 sauté), `task audit` (cargo + deny + pnpm + trivy, redevenu vert), et `task recu` idempotent — l'idempotence contrôlée **après** avoir vu la génération produire `generated.rs`, ce que D8 exige. Couverture domaine+app annoncée à **91,4 %** pour un plancher à 90 %, « en baisse sur les 96,2 % de la fin de M2 », avec l'inquiétude qu'une couverture qui glisse sans que personne l'écrive finisse sous le plancher. **Cette baisse n'a jamais eu lieu** : le chiffre venait de `task test:cover:domain`, qui mesurait autre chose que le gate dont il portait le nom — le gate rendait 96 % ce jour-là comme les précédents. Corrigé en fermant M3, avec le détail ci-dessous ; la ligne reste ici parce qu'une conclusion tirée d'un instrument faux se relit mieux à côté de ce qui l'a produite.

**M3.6 en détail.** Le navigateur devient un device : installable, réveillable, et silencieux quand on le lui a demandé.

**Le service worker est écrit à la main** — pas de `vite-plugin-pwa`, pas de Workbox — et c'est ADR-0017. Le raisonnement tient en deux points : ce fichier décide de ce qui s'affiche sur un écran verrouillé (il rend l'`Envelope` de `push-webpush`, un contrat de fil que rien dans l'OpenAPI ne décrit), donc c'est un fichier qu'on doit **lire** pour lui faire confiance ; et le manifeste de précache généré est un couplage au build, sur un projet dont toute la rigueur consiste à savoir exactement ce qui dérive de quoi. Ce que Workbox achetait ici — précache haché, versionnement de cache, un DSL de routage pour une route — tient dans la trentaine de lignes du fichier. Le coût est assumé et écrit : ni bundlé ni typé, donc `src/lib/sw.spec.ts` charge **ce fichier-là** dans un global de worker bouchonné, plutôt qu'une copie de sa logique.

**Le cache ne touche jamais `/api/`**, et ce n'est pas de la prudence : un `GET /sync` servi depuis un cache rendrait à l'app un backlog que le curseur a déjà dépassé — un second source of truth que personne n'a demandé. Le shell est en network-first, l'inverse (cache-first) servant un bundle périmé à quelqu'un qui est en ligne, alors que le produit entier parle de voir ce qui se passe *maintenant*.

**Le double affichage était le vrai piège.** Un navigateur avec l'app ouverte apprend chaque arrivée **deux fois** — sur son propre flux (§8.1) et par le push. Le worker afficherait donc une notification OS par-dessus le toast que la page allait lever. Il vérifie donc s'il existe un client **visible** : si oui il lui passe l'enveloppe et n'affiche rien, sinon il affiche. « Visible » et pas « ouvert » : un onglet en arrière-plan ne montre rien, donc le considérer comme ayant traité le push avalerait la notification. Et ce détour est aussi ce qui met le push derrière le DND local, que le worker ne peut pas évaluer — il ne tient ni session ni préférence.

**Le DND local est une seconde implémentation, et sa frontière est écrite.** §7.4 l'exige (« sans cette règle, le SSE contournerait le DND sur les clients ouverts ») pendant que M3.1 dit que rien d'autre que le domaine n'a le droit de se demander ce que « DND » veut dire. L'alternative — demander au serveur à chaque arrivée — est un aller-retour pour décider s'il faut dessiner un toast, sur le chemin où la latence *est* la fonctionnalité. Donc `stores/preferences.ts` recopie `Dnd::is_muted` clause pour clause et dans le même ordre, et la copie s'arrête là : elle décide des **popups locaux** et de rien d'autre — aucun push supprimé, rien retiré de l'inbox, aucun autre module qui l'interroge. Les tests reprennent les scénarios des tests Rust, y compris les deux qui se trompent tout seuls : la fenêtre qui traverse minuit appartient au jour où elle **commence**, et l'heure se lit dans le fuseau de la préférence — 23:30 à Tokyo est calme pour une préférence Tokyo et le milieu de l'après-midi pour une préférence Paris, au même instant. `Intl` est la copie navigateur de la base IANA que `chrono-tz` compile dans le serveur, ce qui les fait tomber d'accord sur un changement d'heure sans que ce fichier sache ce qu'est un changement d'heure.

Trois décisions plus petites, notées parce qu'elles se relisent mal :

- **la permission est demandée après `GET /push/webpush`**, jamais avant. Un navigateur ne donne qu'une invite, et la refuser est définitif : la dépenser sur un déploiement qui ne fait pas de Web Push serait la perdre pour rien ;
- **une souscription existante est réutilisée, pas remplacée.** `subscribe()` avec la même clé rend le même endpoint, donc re-souscrire produirait un **second** device pour un seul navigateur. Symétriquement, `disable()` coupe la souscription *avant* de révoquer le device : l'ordre inverse laisse une fenêtre où le serveur envoie un push que plus rien ne route ;
- **l'id du device vit en `localStorage`**, parce que rien d'autre ne distingue deux profils de navigateur — la même personne sur la même machine en Chrome et en Firefox, ce sont deux endpoints et à juste titre deux devices. Le perdre en enrôle un second, visible et révocable dans Devices, ce qui vaut mieux que de deviner qu'ils sont le même et de révoquer le push de quelqu'un d'autre.

Une exception à §10.6 (« aucune couleur brute hors des tokens ») est ouverte ici et c'est la seule : `manifest.webmanifest` et le `<meta name="theme-color">` sont lus par l'OS **avant** qu'aucune feuille de style n'existe. Les deux portent le `--background` du thème sombre en hexadécimal, et le fichier le dit à l'endroit où quelqu'un le lira.

**Ce que M3.6 ne ferme pas, et pourquoi c'est écrit plutôt que tu.** ADR-0016 dit « M3 closes this » : le refresh token quitte `sessionStorage` pour le service worker, qui existe maintenant. Il n'a pas bougé, et la raison est que le déplacement tel qu'imaginé n'achète rien. Un worker est **tué agressivement** ; pour survivre à ça, le jeton devrait vivre en IndexedDB — que la page lit aussi bien que lui, donc un XSS y accède exactement comme aujourd'hui. Le garder en mémoire du worker *protège* réellement, et coûte un aller-retour complet chez l'IdP à chaque éviction, c'est-à-dire souvent : ce serait « tout en mémoire », l'option qu'ADR-0016 a écartée en M1 pour la raison qu'on n'a pas envie de rappeler aux gens de ne jamais recharger. La décision est donc réelle et n'est pas celle qu'ADR-0016 anticipait ; elle mérite son propre enregistrement et son propre incrément, pas un amendement glissé dans un milestone qui parle de push. **ADR-0016 reste `accepted`, pas `superseded`** — et la dette entre en §14.4 sous son numéro plutôt que d'être laissée dans le futur d'un autre document.

**M3.7 en détail.** Le badge « vu par 3/5 » existait ; ce qu'il déplie n'existait pas, et le chiffre lui-même était faux.

**Il fallait un endpoint, et §8 n'en avait pas.** Le panneau comptait les receipts arrivés **sur son propre flux** pendant qu'il était ouvert : quelqu'un qui avait lu la notification la veille était invisible, et le compte repartait de zéro à chaque rechargement d'onglet. `ReceiptRepository::list_for` existait depuis M1 et n'était appelé par personne — un port écrit pour un écran qui ne l'a jamais consommé. D'où `GET /notifications/{id}/receipts`, ajouté à §8 ci-dessus, autorisé **à travers la notification** comme la timeline et les interactions (une frontale ne peut pas lire qui a lu ce qu'elle n'a pas le droit de lire), et qui répond **`null`** là où le canal ne collecte rien — la même orthographe que `POST /receipts` juste à côté, parce que « personne ne l'a lue » et « on n'enregistre pas ça » sont deux phrases différentes et qu'un client qui sait traiter l'une sait traiter l'autre.

**Les noms viennent du serveur, jamais du fil.** L'événement `receipt` de §8.1 porte un `principal` ; une HoverCard a besoin d'une **personne** (§2.4). Le serveur résout donc les profils sur la route, et le client traite l'événement comme un **poke** — exactement la forme du signal local de §9.3bis : la trame ne dit que « regarde à nouveau », une requête en vol à la fois. L'alternative — replier l'événement localement — aurait affiché des ULID bruts à côté des noms venus du serveur, sur le même écran.

**Et le dénominateur n'existe pas toujours.** C'est le bug que seul le fait de le lancer pouvait révéler : sur `cluster-ovh-1`, le canal de démonstration de §11, le serveur répondait `audience: 0` — puis `1` dès qu'une personne avait lu. Alice y accède par le binding `infra-admins`, donc il n'existe **aucune ligne de membership** à compter, et §14.2 dit pourquoi : l'accès par groupe est dérivé à chaque requête, jamais matérialisé, précisément pour que quitter un groupe prenne effet à l'appel suivant. Rien dans le processus ne sait combien de personnes contient `infra-admins`.

Diviser par les lignes qui existent quand même aurait donné « vu par 3/3 » sur un canal que trente personnes lisent : pas une approximation, une affirmation fausse — *tout le monde est au courant* — et une affirmation qu'aucun client ne peut détecter comme fausse. `audience` est donc un **`Option`** : un canal qui n'a que des membres a son total, un canal qui a le moindre binding de groupe n'en a pas, et le badge affiche « vu par 3 » tout court avec la raison dans la carte. La limite résiduelle est nommée là où elle se voit et ne peut pas se fermer ici : une personne qui a le droit de lire par son groupe et n'a rien ouvert n'apparaît nulle part, parce que rien ne sait qu'elle existe.

Quand le total **est** connaissable, il reste un **plancher** : un membre retiré après avoir lu produirait « 2/1 », qui ne se lit pas.

Trois formes plus petites, notées parce qu'elles se relisent mal :

- **une personne, pas un device.** Deux `seen` de la même personne se replient sur le **premier** — ouvrir la même notification sur son portable puis sur son téléphone, c'est l'avoir lue une fois, à la première. C'est aussi ce que §9.3bis avait déjà décidé côté clients locaux (une seule identité device par profil, « pour ne pas compter la machine trois fois ») ;
- **`delivered` compte des écrans**, reste replié sous le compte de personnes, et un receipt **sans device** — l'abonnement logique d'un service account (§5.2bis) — n'est pas un écran ;
- **`SeenTally` a disparu.** Il calculait le numérateur, `Readership` le calcule aussi, et deux noms pour un même fait finissent par diverger. Ses trois tests sont passés sur `Readership` sans perdre une clause, T25 comprise.

**Vérifié en le lançant**, contre un vrai Postgres et le provider de dev : sur un canal à membres, `vu par 1/2` puis `2/2` avec les deux noms et un receipt rejoué qui ne gonfle rien ; sur `cluster-ovh-1`, deux lectrices nommées et `audience: null` ; et `403` pour qui n'est pas dans le canal — carol comme bob avant son membership.

**Fermer M3.** Les sept incréments sont écrits, et rien n'est nommé ici comme manquant : ce que M3 ne prenait pas (la palette `Cmd+K`) était déjà dit plus haut, et ce qu'il a laissé ouvert (le refresh token de M3.6) est une dette numérotée plutôt qu'une phrase.

Portes au vert à la fin de M3 : `task lint:check`, `task test` (564 Rust + 93 web), `task test:critical` (86), `task test:e2e` (54, contre un vrai Postgres — 54 exécutés, zéro sauté), `task test:oidc-compat` (leg `mock`, 3 exécutés et 0 sauté), `task audit` (cargo + deny + pnpm + trivy), et `task recu` idempotent — la génération vue produire `generated.rs` et les quatre fichiers TS **avant** de comparer les empreintes, ce que D8 exige.

Couverture domaine+app : **96,2 %** pour un plancher à 90 %. Et c'est le chiffre qui a produit la seule vraie trouvaille de la fermeture, parce qu'il n'était pas celui qu'on lisait.

**`task test:cover:domain` ne mesurait pas ce que son nom promettait.** Sa description disait « le périmètre que le gate CI applique réellement (≥ 90 %) » et elle lançait `cargo llvm-cov nextest -p notify-domain -p notify-app --summary-only`, ce qui se trompe **deux fois dans le même sens** : la première colonne de ce tableau est la couverture par **région**, quand le gate (`xtask coverage-gate`, qui lit du LCOV) compte des **lignes** ; et `-p` n'exécute que les tests de ces deux crates, quand le gate mesure un run **workspace** — or les suites d'adapters exercent beaucoup de `notify-app`. Les deux erreurs vont dans le même sens, vers le bas.

L'écart n'est pas cosmétique : **90,3 % contre 96,2 %** le même jour, sur la même base. C'est-à-dire un quart de point au-dessus du plancher là où il y en a six. La fin de M3.5 a donc enregistré « 91,4 %, en baisse sur les 96,2 % de M2 » et une inquiétude — « une couverture qui glisse sans que personne l'écrive finit sous le plancher » — sur une baisse **qui n'a jamais eu lieu** : le gate rendait 96,4 % en fermant M1, 96,2 % en fermant M2, et 96,2 % aujourd'hui. Ce qui avait changé, c'était l'instrument.

Un garde-fou local qui affiche autre chose que le gate dont il porte le nom est pire que pas de garde-fou : celui-ci a fait fermer un milestone sur une fausse alerte, et il en aurait produit une par milestone jusqu'à ce que quelqu'un lance les deux commandes côte à côte. `test:cover:domain` **exécute le gate** maintenant — même mesure, même arithmétique, une seule vérité — et le raisonnement est écrit dans la tâche plutôt que dans ce document, là où le prochain à la modifier le lira.

C'est le mode d'échec de D8 par le troisième côté : là une vérification passait sans travail, là le travail passait sans vérification, ici la vérification mesurait autre chose et le disait avec assurance.

**Découpage de M4** :

| # | Livrable | État |
|---|---|---|
| M4.1 | Fédération dans le domaine (§5.3) — `TrustPolicy`, glob **ancré**, `aud` exigée, anti-replay `jti` (T11) | ✅ écrit |
| M4.2 | `POST /auth/token/exchange` — jeton Ed25519, TTL ≤ 15 min, quota 10/min par (issuer, subject) (§8.2) | ✅ écrit — route, adapter, `ReplayGuard` PG, clé et env de dev compris |
| M4.3 | Réconciliation des policies : `[[trust_policy]]` globales et `[[channels.managed.publishers]]` scopées au canal, ces dernières prioritaires (§11) | ✅ écrit — arrivé avec M4.2, qui ne pouvait pas s'échanger sans elles |
| M4.4 | Rôle `platform-admin` (§5.4) — GitOps only, frontière métadonnées/contenu, self-join audité (T30) | ✅ écrit |
| M4.5 | Endpoints `/api/v1/admin/*` — audit log, inventaire, self-join, trust policies, réconciliation (§8) | ✅ écrit — la DLQ manquait faute de sinks, elle est arrivée avec M4bis |
| M4.6 | Web : console plateforme v1 (§10.4bis) et UI trust policies | ✅ écrit |
| M4.7 | Dogfooding : la CI publie ses statuts vers weebo-notify (§3.2, §3.2bis) | 🟡 `notify token exchange` et le workflow écrits ; le workflow n'a jamais tourné contre une vraie Forgejo |

L'ordre n'est pas libre : M4.2 consomme le domaine de M4.1, M4.3 lui donne ses policies, M4.5 consomme le rôle de M4.4, et M4.7 ne peut pas exister avant que M4.2 réponde. Le point de départ est meilleur qu'à M3 : `[[trust_policy]]`, `[[channels.managed.publishers]]` et `[admin] oidc_groups` sont **déjà lus** par la configuration depuis M1.8 et listés par `Config::unhonoured` — le travail est de les honorer, pas de les découvrir.

**Ce que M4 ne peut pas prendre, et qui n'appartient à personne.** §10.4bis fait de la **DLQ des sinks** un des quatre objets de la console v1, et §8 lui donne deux routes (`GET /admin/dlq`, `POST /admin/dlq/{id}/requeue`). Or les sinks sortants (§7.1bis) n'ont **aucun adapter de production** (§14.3) et n'apparaissent dans **aucun milestone** de §14 : ni M1 (« hors périmètre »), ni M2 à M7, qui parlent de sync, de push et de Tauri. Le webhook signé, l'email SMTP, les retries et la DLQ elle-même sont donc spécifiés en détail — contrat HMAC, fenêtre anti-replay de 5 minutes, backoff 30 s → 1 h, abandon à 24 h — et personne ne les écrit.

Deux façons de mentir à ce sujet, et on ne prendra ni l'une ni l'autre : livrer une console dont l'onglet DLQ est vide en permanence et laisser croire que « rien n'est en souffrance », ou retirer la DLQ de §10.4bis et faire disparaître le problème du document. **M4 livre donc les trois autres objets de la console** (audit log, réconciliation, inventaire) et laisse la DLQ dehors, nommée ici — le sujet est un milestone à part entière, pas un onglet, et il faudra décider où il tombe avant M5.

*Décidé depuis* : c'est **M4bis**, ci-dessous, et il tombe juste après M4. Le quatrième objet de la console existe, et il est branché sur une file que quelque chose remplit.

**M4.1 en détail.** La décision de fédération, en pur domaine : des claims que quelqu'un d'autre a signées, des policies que l'opérateur a écrites, et une réponse. Aucun réseau, donc les trois durcissements de §5.3 se testent tous les trois.

**Le glob est le cœur du sujet, et il tient en une méthode.** `repo:weebo/*` doit matcher `repo:weebo/infra` et jamais `repo:weebo-evil/x` : l'implémentation évidente — comparer le préfixe avant l'étoile — fait exactement l'inverse de ce qu'on croit lire. `Pattern` compare donc **le préfixe et le suffixe**, et vérifie qu'ils ne se chevauchent pas, sans quoi `repo:weebo/*:ref:refs/heads/main` matcherait `repo:weebo/:ref:refs/heads/mai`. Une bibliothèque de glob aurait apporté `?`, les classes de caractères et `**` — trois façons de plus d'écrire un motif qui matche plus large que ce que son auteur croyait, pour un besoin que §11 exprime avec une étoile.

Deux motifs sont **refusés à la construction** plutôt que gérés : le vide, et `*` tout seul. Un `*` de sujet fait confiance à quiconque peut ouvrir une pull request sur le dépôt de l'issuer ; qui veut vraiment ça doit écrire quelque chose qui ne peut pas être un lapsus.

**Le port promettait ce que son type interdisait.** `FederationVerifier::verify_ci_token` rendait des `VerifiedClaims` — la forme du login humain —, et sa documentation annonçait les deux contrôles de T11. Or `VerifiedClaims` ne porte **ni `aud` ni `jti`** : aucune implémentation n'aurait pu faire ces contrôles, et aucun appelant n'aurait pu les faire après. Ce n'est pas un oubli de câblage, c'est une règle écrite en prose à côté d'un type qui la rend inexprimable — la même famille que le `PUSH_PAYLOAD_HINT` de M3.5 et que la matrice `oidc-compat` de §15. Le port rend maintenant des `FederatedClaims`, et **ne décide plus rien** : il rapporte les audiences, le domaine refuse. Un seul endroit décide « ce jeton n'est pas pour nous », et c'est un endroit sans I/O et avec des tests.

**L'anti-replay est un port à part.** `IdempotencyStore` existait et ressemblait à ce qu'il faut, mais sa sémantique est l'inverse : il **rejoue** la réponse stockée, ce qui est juste pour un publish qu'un client a retenté après un timeout, et exactement faux ici — rejouer rendrait une seconde copie du jeton frais à qui présente à nouveau un ID token volé, transformant « il vaut un échange » en « il en vaut autant qu'on veut, tous identiques ». D'où `ReplayGuard`, dont la seule méthode **teste et enregistre en un appel** : en deux, deux jobs présentant le même jeton volé liraient tous les deux « jamais vu ».

**Une policy de canal l'emporte, et c'est écrit plutôt que déduit.** §11 dit que les blocs `publishers` d'un canal priment sur les `[[trust_policy]]` globales. La lecture tentante — « la plus étroite gagne » — est une *autre* règle qui tombe d'accord la plupart du temps, et diverge le jour où quelqu'un écrit une globale plus étroite que la locale. Le `TrustPolicy` porte donc un booléen `channel_scoped`, et dès qu'une policy scopée matche, les globales sont **écartées** : un canal qui nomme ses publishers a dit qui ils sont.

**M4.2, la moitié qui est écrite.** `ExchangeToken` et la porte d'authentification qui accepte ce qu'il émet. Ce qui reste est nommé plus bas plutôt que présenté comme fini.

**L'ordre des contrôles est la sécurité, et chaque position a sa raison.** Signature, quota, policy, anti-replay, mint. Deux placements ne sont pas interchangeables :

- **le `jti` est consommé après la policy et avant le mint.** Le consommer plus tôt — dès qu'un jeton se présente — donnerait à n'importe qui une façon de **verrouiller un vrai job hors de son propre échange** : il suffirait de présenter un jeton portant le `jti` de la victime pour le griller. Le consommer plus tard laisserait une fenêtre où deux requêtes concurrentes frappent toutes les deux. Un test vérifie précisément la première clause : un refus ne consomme pas l'id sur lequel il a été refusé ;
- **le quota est indexé sur les claims vérifiées**, donc après la signature — §8.2 limite « par (issuer, subject) » et aucun des deux n'existe avant. Ce qui protège la vérification elle-même est le plafond global devant, pas celui-ci.

**Les patterns sont résolus au mint, pas à l'usage.** Le jeton nomme `ci-infra` et `ci-web`, jamais `ci-*`. C'est ce que le type `MintedToken` de M1 supposait déjà, et la raison tient à §11 : la configuration se recharge à chaud, donc deux requêtes d'un même pipeline pourraient légitimement obtenir deux réponses différentes si chacune rejouait le match. Résoudre une fois écrit la réponse. Conséquence assumée : un canal créé après l'échange n'est pas couvert par le jeton en cours — quinze minutes, puis il l'est.

Cela a demandé `ChannelRepository::all_ids`, et sa documentation dit ce qu'il n'est pas : une lecture **non scopée**, donc jamais un chemin d'autorisation. Elle répond « qu'est-ce qui existe », pas « qu'as-tu le droit de voir » — mélanger les deux est la façon dont un inventaire devient une fuite.

**Une policy qui matche mais ne couvre aucun canal existant est un refus**, pas un jeton vide. Émettre un jeton qui ne peut rien faire repousse l'erreur de configuration jusqu'au job, qui la découvrira un 403 à la fois, très loin de la ligne de TOML qui l'a causée.

**Deux jetons porteurs sur le même en-tête, distingués en essayant.** Un access token OIDC et un jeton CI de notre cru arrivent tous deux en `Authorization: Bearer`, et rien dans l'en-tête ne les sépare — une clé d'API a au moins son préfixe `wnk_`. Lire `iss` dans un JWT **non vérifié** pour choisir le vérificateur serait router sur une donnée fournie par l'attaquant ; on essaie donc le vérificateur maison d'abord, et l'échec coûte une lecture d'en-tête qui s'arrête sur l'algorithme. Un déploiement sans `[federation]` ne fait pas l'essai du tout, d'où un `Option` plutôt qu'un double qui refuse (§14.3) : ce n'est pas un use case qui découvre une capacité absente, c'est le chemin le plus chaud de l'API qui décide s'il y a une seconde porte.

Le grant est **lu sur le jeton**, jamais re-dérivé, pour la même raison que la résolution au mint.

**L'allowlist d'issuers est une frontière de sécurité, pas une commodité.** Choisir quel JWKS aller chercher oblige à lire `iss` dans un jeton **avant** d'en vérifier la signature — c'est inévitable, on ne vérifie pas une signature sans savoir de quelle clé il s'agit. Ce qui rend ça sûr est que l'ensemble des issuers est **figé au démarrage depuis les trust policies** : un issuer que personne n'a écrit dans la config est refusé avant tout appel réseau. Sans ça, `POST /auth/token/exchange` — accessible **sans credential**, par construction — serait une primitive de SSRF pointée sur tout ce que le cluster peut joindre. Un test le verrouille.

Le refus est d'ailleurs le même que pour une mauvaise signature : distinguer « on ne fait pas confiance à cette forge » de « ta signature est fausse » dirait à qui sonde quels issuers sont configurés.

**Ce que l'adapter reprend, et ce qu'il n'a pas réécrit.** Un jeton de forge *est* un jeton OIDC, et §4 met déjà « Authentik **et issuers CI fédérés** » dans le crate `oidc`. Il y a donc un `OidcVerifier` par forge, ce qui hérite gratuitement de tout ce que §5.1 refuse de faire : croire l'`alg` de l'en-tête, refetcher un key set à volonté, suivre un document de découverte qui nomme un autre issuer. La seule chose ajoutée est `verify_federated`, qui diffère en trois points, tous parce qu'un jeton fédéré est **rapporté** et non **accepté** : l'`aud` n'est pas validée là (le domaine refuse), le `jti` est extrait (sans quoi l'anti-replay serait inimplémentable), et les groupes sont ignorés (une forge n'a aucune notion de l'annuaire de cette instance).

**Le `ReplayGuard` Postgres est un `INSERT … ON CONFLICT DO NOTHING`, et c'est tout le mécanisme.** « Lire si l'id est connu, puis l'écrire » fait deux instructions, donc une course : deux jobs présentant le même jeton volé au même instant liraient tous les deux « jamais vu ». Une clé primaire ne se course pas.

**La clé de signature est une addition à §11**, faite ci-dessus : §5.3 exigeait de l'Ed25519 et §11 n'avait aucun bloc pour dire où vit la clé privée. Elle ne peut pas être générée au démarrage — un replica signerait ce qu'un autre ne saurait pas vérifier, et la panne serait intermittente, dépendante du load balancer, et ressemblerait à un pipeline instable. D'où `[federation] signing_key_file`, sur le modèle exact du `vapid_key_file` de M3.5 : minté par `task dev:setup` via `xtask`, gitignoré, refus de démarrer si le bloc est là et la clé illisible. La moitié publique est **dérivée de la privée** plutôt que configurée à côté : deux fichiers qui doivent correspondre sont deux fichiers qui peuvent cesser de correspondre.

**Vérifié en le lançant**, contre un vrai Postgres et le provider de dev — qui tient lieu de forge, parce que ce que l'échange consomme est un ID token avec `aud`, `exp` et `jti`, et que le mock en émet exactement comme une forge. Le chemin complet : login PKCE → `POST /auth/token/exchange` → jeton EdDSA de 15 minutes nommant `ci-infra` → publish, avec un `author` `ci_job` portant l'issuer et le subject. Et les trois refus : **rejouer le même ID token → 401**, publier dans un canal hors policy → **403**, lire l'inbox avec un jeton `publish` seul → une page **vide** (0 entrée, là où alice en voit 8) — c'est-à-dire la clause T12 tenue pour un jeton fédéré, et pas un refus qui aurait pu cacher une lecture.

Une trouvaille mineure du même passage : `[[channels.managed.publishers]]` continuait d'être annoncé « non honoré » au démarrage alors qu'il l'était. Un avertissement qui ment sur ce que le build fait est exactement ce que `Config::unhonoured` existe pour empêcher.

**M4.3 est arrivé avec M4.2**, parce qu'un échange sans policies n'échange rien. La seule décision qui valait d'être prise est celle que §11 laissait implicite : un bloc `publishers` devient une policy **sur le canal sous lequel il est écrit**, et rien d'autre. Un `publishers` capable de nommer d'autres canaux serait une policy globale déguisée en locale, et la précédence de §11 n'aurait plus de sens.

**M4.4 en détail.** Le rôle plateforme, et la frontière qu'il ne franchit pas.

**Il est dérivé, jamais stocké** — exactement comme l'accès par groupe de §14.2 et pour la même raison : c'est `[admin] oidc_groups` intersecté avec ce que ce jeton-ci dit. Quelqu'un retiré du groupe perd le rôle à son prochain jeton, sans rien à réconcilier et sans ligne à oublier. Et il vaut `false` pour une clé d'API, un service account et un job CI, pas par un contrôle que chaque handler doit penser à écrire mais parce que les trois constructeurs de `Caller` l'écrivent : §5.4 le réserve aux users OIDC, donc la clé personnelle d'un admin administre ses canaux et jamais l'instance.

**Une liste vide n'accorde rien**, ce qui fait qu'un opérateur qui n'a pas dit qui administre la plateforme n'a pas dit « tout le monde ». Même propriété que T29 côté canaux.

**La frontière est le sujet, et « les admins voient tout » en est l'inverse.** §5.4 la pose en une phrase — métadonnées et opérations, jamais le contenu — et ce n'est pas de la prudence : c'est la seule ligne qui garde vraie la règle de §2.4. Les profils ne sont visibles « qu'à travers un canal commun », donc un rôle capable de lire n'importe quel canal rendrait cette phrase fausse pour tout le monde à la fois, en silence, aussi longtemps que quelqu'un porte le rôle. L'inventaire rend donc des **fiches** — id, origine, compteurs — et le type est la garantie : `ChannelCard` n'a aucun champ où une notification pourrait tenir, donc aucune édition future ne l'élargit en inbox par accident. Les membres y sont un **nombre** et pas une liste, pour ne pas mettre l'identité de quelqu'un sur l'écran d'un canal qu'il ne partage pas avec le lecteur.

**Le self-join est l'échappatoire qui rend la frontière tenable**, et sa valeur est d'être écrite. Un admin en incident a parfois besoin de lire le canal ; en faire un **acte délibéré et enregistré** est toute la différence entre un rôle qui lit tout sans bruit et un rôle qui lit ce qu'il a demandé, noir sur blanc. Il n'accorde que `subscribe` : lire un incident n'est pas une licence pour parler au nom d'une équipe. Sur un canal managé la réconciliation reprendra le membership (§11), et c'est le résultat honnête — la config possède la liste, donc un accès durable s'écrit dans la config, pendant que l'entrée d'audit garde trace du regard.

Chaque use case revérifie le rôle, non parce que la couche HTTP l'oublierait mais parce qu'un contrôle qui n'existe que dans une table de routes est un contrôle qu'une seconde table de routes peut ignorer.

**Vérifié en le lançant** (alice dans `platform-admins`, bob dans `dev-team`) : inventaire à 200 pour alice et **403 `platform.role_required`** pour bob ; un canal de bob qu'alice ne rejoint pas apparaît dans son inventaire comme fiche et lui rend **zéro notification** là où bob en voit une ; self-join refusé à bob, **204** pour alice, après quoi elle lit la notification — et **403 quand elle essaie d'y publier**. L'entrée `platform.channel.self_join` est dans l'audit log.

**M4.6 en détail.** Trois panneaux, chacun répondant à une question avec laquelle quelqu'un arrive vraiment : *cette instance est-elle configurée comme on le croit* (canaux), *pourquoi ce pipeline a-t-il été refusé* (trust policies), *que s'est-il passé et qui l'a fait* (audit). Pas de séries temporelles : §13 les laisse à Grafana, et les dupliquer donnerait un écran auquel personne ne se fie parce qu'il contredit celui qui a les données.

**Comment l'app apprend qu'elle a le droit d'afficher ça.** Par **une sonde**, et délibérément pas par un drapeau posé sur la session. Le serveur dérive le rôle par requête (§14.2), donc il est la seule chose capable de répondre ; demander une fois à l'ouverture est à la fois le moins cher et le plus honnête — ce que la navigation montre est alors exactement ce que l'API autorise, et non une copie qui peut dériver. Le `403` que reçoit presque tout le monde **est la réponse** et non une erreur à afficher, ce qu'un test verrouille dans les deux sens : un 403 se lit « pas admin » sans message, un 503 ne se lit **pas** comme un refus de rôle.

Un troisième état existe et compte : `null`, « on n'a pas encore demandé ». Une entrée de menu qui clignote entre absente et présente est pire qu'une qui attend.

**Les trust policies sont en lecture seule, et c'est tout ce qu'un écran peut honnêtement en dire** : §11 les possède, donc un bouton d'édition proposerait ce que la prochaine réconciliation reprendrait. Ce que la table montre en revanche est ce qui manquait pour diagnostiquer un refus : le badge `canal` vs `globale`, c'est-à-dire la précédence de §11 — la première chose à regarder quand une policy « devrait » matcher et ne matche pas.

Une trouvaille en câblant le client : `GET /admin/audit` acceptait quatre filtres que **le document OpenAPI ne décrivait pas**, parce qu'ils étaient déclarés sur le handler (`web::Query`) et pas dans `#[utoipa::path]`. Le client généré rendait donc `query?: never` : un filtre que le serveur accepte et que le document tait est un filtre qu'aucun client ne peut envoyer. C'est ADR-0009 pris à revers — les clients viennent du contrat, donc le contrat doit décrire ce qui est servi.

**`GET /admin/reconciliation` a demandé une décision de forme.** L'état d'une passe n'est ni du stockage ni un système externe : c'est ce que *ce processus* a fait il y a une minute, dans sa propre mémoire. Un port aurait voulu dire un adapter, un double et un trait pour une valeur qui ne quitte jamais le replica — d'où un simple emplacement partagé, écrit par la boucle de fond et lu par la console.

Écrit par la **boucle** et non par le réconciliateur, et c'est la moitié qui compte : une passe qui échoue ne doit pas remplacer la dernière qui a marché par rien du tout, et seul l'appelant sait laquelle des deux vient de se produire.

**Et c'est par replica, ce que l'écran dit.** Contrairement à la purge, §11 ne réserve pas la réconciliation au leader : *tous* les replicas l'appliquent. Derrière un load balancer, deux lectures peuvent donc légitimement différer d'un tick. L'annoncer comme un état d'instance aurait été le mensonge confortable ; le champ est documenté comme la réponse d'un replica, et la console le répète.

`null` quand aucune passe n'a fini, plutôt que des zéros : la passe de démarrage tourne avant l'ouverture du socket, donc un appelant ne devrait jamais le voir — ce qui est précisément pourquoi c'est modélisé. « 0 canaux, 0 comptes » se lit comme une instance saine qui a appliqué sa configuration, c'est-à-dire l'inverse de ce que cet état veut dire.

Le seul chiffre **actionnable et silencieux autrement** y est aussi : les liaisons par `email` ou `oidc_sub` en attente d'une première connexion (§14.1, M1.8). Sans un endroit où les voir, le symptôme est « alice dit qu'elle ne voit pas le canal » et rien pour le relier à sa cause.

**Ce que M4.5 ne servait pas**, et c'était délibéré : la **DLQ des sinks**. §10.4bis en fait un des quatre objets de la console et §8 lui donne deux routes, mais aucun sink n'existait alors (§14.1) — servir un endpoint qui répond « rien n'est en souffrance » depuis une file que personne ne remplit est un voyant vert câblé sur rien, ce qui est pire que pas de voyant. Les deux routes sont arrivées avec M4bis, en même temps que la file.

**M4.7 en détail, et sa limite d'abord.** Le workflow existe et **n'a jamais tourné contre une vraie Forgejo** — il n'y en a pas dans cet environnement. C'est écrit ici, en tête, parce que le reste du document passe son temps à dénoncer les tics verts qui ne prouvent rien (D7, D8, D10) et qu'un fichier de CI commité sans avoir jamais tourné en est un candidat naturel.

Ce qui est vérifié est la moitié qui compte : **les deux commandes que le workflow lance**. `notify token exchange` et `notify update` ont été exécutées contre un vrai serveur et un vrai provider, avec un jeton frappé, un `author` `ci_job` et une notification qui atterrit. Ce qui reste non observé tient en deux lignes que seule une forge peut répondre : que `id-token: write` rende un jeton, et que son `aud` nomme weebo-notify. **Les deux échouent bruyamment** si elles sont fausses — pas de jeton donne une variable vide et une étape en échec, et un mauvais `aud` est précisément ce que §5.3 refuse en 401. La première exécution sur une vraie forge est le test.

**La démonstration est l'absence de secret.** Il n'y a pas de clé d'API dans le dépôt, pas dans le coffre de la forge, et rien à faire tourner : le runner détient déjà un jeton disant quel dépôt et quelle ref il est, et l'échange en fait quinze minutes de droit de publier sur un canal. Côté instance réceptrice, le travail d'un opérateur est un bloc de TOML.

Deux formes valent d'être notées :

- **`update` et pas `publish`**, sur l'id du run : le même appel crée au premier passage et révise au re-run, donc un pipeline relancé amende sa propre notification au lieu d'en ouvrir une seconde (§2.2). C'est le cas d'usage que §2.2 décrit en premier, et il tombe juste sans que le workflow ait à savoir dans lequel des deux cas il est ;
- **le job entier est sauté quand l'instance n'est pas configurée** (`vars.WNK_URL`), parce que c'est l'état normal d'un fork et qu'échouer le pipeline de quelqu'un d'autre pour une notification qu'il n'a pas demandée serait le mauvais défaut.

**Un manque de §9.1 nommé, puis soldé** : le croquis y écrit `notify publish --action "view:Runbook:https://…"`, et le drapeau n'existait pas. Le workflow est parti sans bouton « voir le run » plutôt qu'avec un drapeau imaginaire, puis D11 l'a ajouté et le workflow l'a récupéré. `--image` a suivi, et a fait tomber D14 : au-dessus de 48 KiB il n'y a aucun chemin, pour personne.

**Découpage de M4bis** :

| # | Livrable | État |
|---|---|---|
| M4bis.1 | Domaine + use cases — `SinkId`, `RetryPolicy`, `DrainSinks`, les deux opérations DLQ, l'enfilement dans le `Fanout` | ✅ écrit |
| M4bis.2 | `sinks-webhook` (HMAC + vérificateur de référence) et `sinks-email` (SMTP) — les deux premiers adapters de `SinkDispatcher` (§14.3) | ✅ écrit |
| M4bis.3 | `SinkQueue` Postgres (bail, pas prise), boucle de drain leader-only, `[[channels.managed.sinks]]` et `[sinks.smtp]` honorés | ✅ écrit |
| M4bis.4 | `GET /admin/dlq`, `POST /admin/dlq/{id}/requeue`, quatrième panneau de la console (§10.4bis) | ✅ écrit |
| M4bis.5 | T17 de bout en bout en suite `critical`, conformance contre un vrai Postgres, portes | ✅ écrit |

L'ordre n'est pas libre : M4bis.3 consomme les adapters de M4bis.2, M4bis.4 consomme la file de M4bis.3, et T17 ne peut se jouer entier qu'une fois les quatre en place. Le point de départ est le meilleur de tous les milestones jusqu'ici : `SinkDispatcher`, `SinkQueue`, `SinkTarget` et `DeadLetter` étaient **écrits depuis M1**, avec leurs doubles in-memory et leurs tests. Le travail a été de les honorer — et de découvrir que deux d'entre eux disaient la mauvaise chose.

**M4bis.1 en détail.** Deux valeurs et un job, plus une décision qui a redessiné le port.

**Le secret n'entre jamais dans la file** (ADR-0018). Le `SinkDelivery` de M1 portait son `SinkTarget`, donc son secret HMAC : sérialisé dans une ligne, il aurait atterri dans chaque dump, chaque sauvegarde et chaque écran d'où un opérateur peut pointer une ligne — pour un credential qui appartient à quelqu'un d'autre, sur une base que §6.1 garde délibérément sans chiffrement applicatif. Une ligne **nomme** donc son sink, et le registre (la config, en mémoire) rend la cible au moment de l'envoi. Deux propriétés tombent gratuitement avec : une rotation de secret s'applique à ce qui est déjà en file, et un sink retiré de la config cesse d'être réessayé au lieu d'être réessayé pendant vingt-quatre heures vers un endroit que plus personne ne sert.

D'où l'identité, que §11 ne donnait pas : `kind:destination`. Elle est stable à travers un redémarrage, sans credential — une URL portant un userinfo est refusée là où la config est lue —, et **lisible dans la DLQ**, qui est exactement l'écran où quelqu'un a besoin de savoir *quel endpoint* est coincé. Conséquence assumée et écrite : un sink repointé sur une nouvelle URL est un nouveau sink.

**Le calendrier est une valeur, pas une boucle.** « Backoff exponentiel 30 s → 1 h, abandon après 24 h » est une phrase de §7.1bis et trois erreurs faciles en code : un délai qui cesse de croître au mauvais endroit, un plafond appliqué avant le doublement, et un abandon mesuré depuis le mauvais instant. `RetryPolicy` est donc un type du domaine avec ses tests, et l'abandon court **depuis le premier échec** et non depuis la publication : un consommateur qui tombe trois heures après une publication chargée a sa journée entière à partir du moment où l'ennui commence.

**Le classement, lui, reste dans l'adapter.** Seul ce qui a parlé HTTP sait qu'un `503` veut dire « plus tard » et un `410` « jamais ». Le use case ne fait ni l'un ni l'autre : il met les deux bout à bout et écrit le résultat. C'est la même frontière que §7.4 trace entre « le publisher choisit l'insistance » et « le destinataire choisit ce qui le réveille ».

**Une publication met en file, elle n'envoie pas.** Appeler l'endpoint de quelqu'un d'autre avant de répondre au publisher mettrait sa latence, sa panne et son handshake TLS devant un job CI qui demandait juste qu'on note une ligne — et le premier timeout rendrait un pipeline rouge pour une raison sur laquelle personne ne peut agir. Le prix est écrit là où il se voit : un sink est livré dans l'intervalle du drain (5 s) et non dans un aller-retour.

**Et l'événement est reconstruit, pas relu.** Le `Fanout` tient déjà la notification, la révision et le curseur — les trois valeurs dont le store compose l'événement qu'il vient d'ajouter. Le redemander au store serait une lecture par publication pour retrouver ce que l'appel a en main ; un test de conformance contre un vrai Postgres tient les deux formes ensemble, ce qui est la garantie que la reconstruction n'a pas dérivé.

**M4bis.2 en détail.** Deux adapters, et le webhook est le seul endroit du produit qui soit une **API publique pour un logiciel qu'on ne verra jamais**.

**Le vérificateur de référence est livré avec le signeur.** `verify()` vit dans le même module que `sign()`, testé par les mêmes tests. Un contrat qu'un consommateur doit deviner est un contrat implémenté de trois façons subtilement différentes ; celui-ci se copie. La comparaison y est à temps constant, parce que l'alternative fuit le digest un octet de timing à la fois vers quiconque a le droit de POSTer là.

**Le timestamp est dans la matière signée**, et c'est ce qui rend un rejeu détectable *du tout* : une signature sur le seul corps est valide pour toujours. Un consommateur qui refuse au-delà de 5 minutes obtient l'anti-replay en prime d'une comparaison qu'il allait faire de toute façon (T17).

Trois refus plus petits, notés parce qu'ils se relisent mal : **aucune redirection n'est suivie** (signer pour un hôte et livrer à un autre est exactement ce qu'un `302` ferait) ; **un `410` ne se réessaie pas** (vingt-quatre heures de retries n'aident personne et retardent le moment où un humain voit la ligne) ; et **il n'y a pas de garde SSRF dans l'adapter**, parce que l'URL vient de `[[channels.managed.sinks]]` — la vérification de schéma et le refus des credentials dans l'URL sont là où le fichier est lu, au démarrage, à côté du refus d'un `type` inconnu.

Côté mail, la décision qui compte est **ce qu'on n'envoie pas** : seules les révisions deviennent un mail. Un `receipt`, une `interaction`, un `tombstone`, une `preference` sont l'affaire d'un client et la boîte de personne — « alice a lu ta notification » est un badge, pas un courrier. Un webhook les reçoit toutes les cinq parce qu'un programme ignore ce qui ne le concerne pas ; une boîte aux lettres, non. Et **pas de partie HTML** : le corps est du markdown par contrat (§2.3), et une alternative HTML voudrait dire rendre du markdown fourni par un publisher dans un client mail, c'est-à-dire la surface de sanitisation la plus indulgente qui soit, pour quelques mots en gras.

**M4bis.3 en détail.** La file, et la seule chose qui n'est pas évidente dedans.

**Prendre est un bail, pas un retrait.** Un drain qui supprimerait la ligne en la ramassant perdrait toutes les livraisons en vol à la seconde où le leader est tué — un trou silencieux dans une promesse *at-least-once*, et du genre qui ne se manifeste que par « le webhook n'arrive parfois pas ». La ligne est donc **baillée** : elle redevient due toute seule à l'expiration, et le doublon que ça coûte au pire est exactement ce que `X-Weebo-Delivery` existe pour rendre supportable. La prise est **une seule instruction** (`UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED) RETURNING *`) : lire puis écrire est une course que deux leaders — il en existe deux pendant chaque bascule — gagnent tous les deux.

`SKIP LOCKED` plutôt qu'une attente : un drain qui bloquerait sur la ligne d'un pair sérialiserait toute la file derrière un seul consommateur lent.

**Enterrer est transactionnel.** Une livraison à la fois due *et* abandonnée serait renvoyée par le prochain drain pendant que la console la liste comme perdue.

**Le requeue rend une chance, pas la journée passée à échouer** : `first_failed_at` repart à `NULL`. Le garder ferait ré-enterrer la livraison au tout premier échec suivant, c'est-à-dire l'inverse de ce que demande l'admin qui appuie sur le bouton.

**Le drain a son propre intervalle**, 5 s contre 60 pour la purge et le digest — parce que cet intervalle *est* la latence de livraison, pas un rythme de ménage. Il est leader-only pour la raison des deux autres : chaque replica qui drainerait livrerait chaque webhook une fois par replica.

**M4bis.4 en détail.** Deux routes et un panneau, dont la seule vraie question est ce qu'ils ne montrent pas.

`GET /admin/dlq` nomme un canal, un sink et une erreur, et s'arrête là. L'événement qui n'a pas pu être livré est dans la ligne de file et **n'est pas servi** : un admin non membre du canal lirait sinon ses notifications à travers l'échec d'un webhook, ce qui est exactement la porte que §5.4 ferme. Un test côté client le verrouille aussi, parce que c'est le genre de champ qu'on ajoute « pour déboguer ».

Un requeue sur une entrée absente rend **409 `dlq.gone`** et pas 204 : une console qui annoncerait un succès pour un requeue qui n'a rien remis laisserait un opérateur attendre une livraison que personne n'a programmée. Variant dédié plutôt que `notification.gone` — la notification, elle, se porte très bien.

**M4bis.5 en détail, et les trois choses que le fait de le lancer a sorties.**

T17 se joue en trois endroits, un par question qu'il pose : la signature est vérifiée par un **consommateur écrit ailleurs** (un script Node, pas notre code), le rejeu à `t − 6 min` est refusé, et la séquence complète — échec, backoff 30 s puis 60 s, abandon après 24 h, requeue, livraison — est **un seul test**, parce que ce qui est promis est la séquence et non ses étapes prises séparément.

**Vérifié en le lançant**, contre un vrai Postgres, le provider de dev et un consommateur Node qui recalcule le HMAC lui-même : publication → livraison en 5 s avec `signature_ok: true` et un timestamp dans la fenêtre → consommateur qui répond `410` → DLQ avec l'endpoint nommé → `requeue` **204** puis **409 `dlq.gone`** au second → redelivrée et acceptée, **avec le même `X-Weebo-Delivery`** qu'à la première tentative. Et `403 platform.role_required` pour bob, qui n'a pas le rôle.

Trois trouvailles, dont une qui ne concerne pas les sinks du tout :

- **`TestLeaderElection::release()` démettait le replica.** Le double transformait donc chaque job en *leader-pour-une-passe* : un test qui en lance deux voyait la seconde ne rien faire, pour une raison sans rapport avec ce qu'il testait. Le vrai `PgLeaderElection` relâche son advisory lock et le reprend au tick suivant ; le double dit maintenant la même chose, et perdre l'élection reste `depose()`. C'est un double qui divergeait du contrat de son port — la même famille que le `PUSH_PAYLOAD_HINT` de M3.5, vue depuis les tests ;
- **une livraison orpheline est apparue toute seule.** Le premier démarrage a logué « dropping a delivery whose sink left the configuration » : une ligne laissée par la suite de conformance, dont le sink n'existe pas dans la config du serveur. C'est le chemin de M4bis.1 qui s'exécute pour de vrai, et il s'exécute *en le disant* — ce qui est la moitié qui compte, parce qu'une file qui jette en silence est le seul comportement qu'une file n'a pas le droit d'avoir ;
- **`cargo deny` a refusé une licence** : `quoted_printable`, sous `0BSD`, arrive avec `lettre`. Permissive, approuvée OSI, et strictement plus permissive que plusieurs licences déjà sur la liste — ajoutée avec sa raison, une ligne, comme les autres.

Portes au vert à la fin de M4bis : `task lint:check`, `task test` (710 Rust + 103 web), `task test:critical` (**101**, contre 86 à la fin de M3), `task test:e2e` (**59**, contre un vrai Postgres — 59 exécutés, zéro sauté), `task test:oidc-compat` (leg `mock`, 3 exécutés et 0 sauté), `task audit` (cargo + deny + pnpm + trivy), et `task recu` idempotent — l'idempotence contrôlée **après** avoir vu la génération produire `generated.rs` et les quatre fichiers TS, ce que D8 exige. Couverture domaine+app : **95,8 %** pour un plancher à 90 %, mesurée par le gate lui-même (D10).

**Ce que M4bis ne prend pas**, nommé plutôt que passé sous silence : la métrique `sink_dlq_depth` de §13. Le port répond déjà à la question (`dead_letter_depth`, testé contre un vrai Postgres), et il n'y a **aucun exporteur Prometheus dans ce build** — §13 en entier attend son incrément. Câbler une jauge sans `/metrics` serait un compteur que personne ne lit ; la faire vivre est le travail de l'observabilité, pas celui des sinks. *Décidé depuis* : c'est **M4ter**, ci-dessous.

**Découpage de M4ter** :

| # | Livrable | État |
|---|---|---|
| M4ter.1 | Port `Metrics` + `NoMetrics`, les labels fermés (`PushResult`, `StreamKind`) | ✅ écrit |
| M4ter.2 | `metrics-prometheus` — les six familles de §13, leurs buckets et le rendu OpenMetrics | ✅ écrit |
| M4ter.3 | Les sites d'enregistrement, `GET /metrics`, et le registre unique dans la composition | ✅ écrit |
| M4ter.4 | Page `ops/observability` — ce qu'une scrape dit, ce qu'elle tait, et comment écrire l'alerte | ✅ écrit |

L'ordre est celui de M4bis pour la même raison : rien au-dessus du port ne sait qu'il existe un registre, donc le port se pose avant l'adapter et l'adapter avant les appelants.

**Un port et pas un enregistreur global.** La bibliothèque évidente ici expose un registre statique et des macros qui écrivent dedans, et c'est le choix qui n'a pas été fait : il rendrait *toute* mesure invisible à la relecture — n'importe quelle ligne du produit pourrait compter quelque chose —, et il rendrait les tests dépendants d'un état de processus partagé. `Metrics` est donc un trait comme les autres ports, la racine de composition décide s'il pointe vers un registre Prometheus ou vers nulle part, et `NoMetrics` est le défaut des tests. Le corollaire est une propriété qu'on peut vérifier en lisant : les endroits qui touchent une métrique se listent — deux handlers d'ingestion, le handler de sync, le garde de connexion, le registre de transports, la boucle de drain — et il n'y en a pas d'autre.

**Les métriques sont enregistrées par les adapters, jamais par les use cases.** Tout ce que publie §13 est un fait sur *le processus* — combien de connexions ce replica tient, combien de temps un relais a mis, quelle est la profondeur de la DLQ — et non sur le domaine. Un use case qui chronométrerait sa propre latence serait un use case qui sait qu'on le regarde, et sa suite in-memory porterait un handle de métriques pour rien. La seule qui ressemble à un fait de domaine, `notifications_published_total`, est comptée **là où la requête est acceptée** : §8.3 fait converger toute l'ingestion sur deux handlers, donc les compter compte tous les publish — `curl`, CLI, job de CI. Un rejeu idempotent n'en est pas un : il a publié une fois, et il a été compté ce jour-là.

**`realtime_connections` est tenue par un garde, pas par deux appels.** La moitié intéressante est la décrémentation : un stream se termine quand le client s'en va, quand la tâche est abattue à l'arrêt, ou quand le log devient illisible — et un handler qui décrémenterait sur son seul chemin de succès fuirait une connexion sur chacun de ces trois cas. La jauge sert aussi à l'HPA (§11), donc une fuite là-dedans est une instance qui ne redescend jamais. **Et c'est en le lançant que la première version s'est révélée fausse.** Écrite d'abord, la tâche n'attendait que les pokes du bus : un client tué a survécu à un keep-alive *et* à une première publication, et n'est redescendu qu'à la **seconde** — donc sur un canal silencieux la nuit, une connexion morte serait restée comptée toute la nuit, avec l'HPA qui lit ce nombre. Le commentaire du code affirmait le contraire (« un keep-alive suffit ») ; c'est la scrape d'un vrai serveur qui a tranché, pas la relecture.

Le correctif est un `select!` : la tâche attend le poke **ou** la fermeture du récepteur, donc le départ s'apprend sans que personne ait à publier. Re-mesuré sur le même scénario : **50 s**, soit deux keep-alives — le premier écrit dans un socket mort réussit encore, c'est le second qui échoue — et aucune publication dans l'histoire. Le WebSocket, lui, garde le comportement d'origine : il n'apprend qu'en écrivant, parce que ce handler jette le flux entrant, donc sur une instance sans trafic un socket mort reste compté jusqu'à la prochaine publication. Écrit ici et dans la page d'ops plutôt que corrigé à la va-vite : lire le flux entrant, ce serait aussi répondre aux pings, c'est-à-dire changer le comportement du transport pour une métrique.

**`sink_dlq_depth` est la réponse du leader, et l'absence y veut dire quelque chose.** La boucle de drain la rafraîchit après une passe qu'elle a menée, et personne d'autre ne l'écrit — donc sur un replica qui ne mène pas, la famille reste **vide** au lieu de rapporter zéro. Une série absente et une série à zéro sont deux phrases différentes : la première dit « ce replica ne sait pas », la seconde dit « rien n'est coincé », et une seule des deux est vraie sur un suiveur. L'alerte se formule donc sur le `max` entre replicas, ce que la page d'ops écrit. Symétriquement, `dlq_depth` **remplace** la famille au lieu de s'y ajouter : un canal que quelqu'un vient de vider doit cesser d'être rapporté, pas garder sa dernière valeur jusqu'au prochain redémarrage — sinon l'alerte continue de sonner pour une file déjà traitée.

**Une livraison est chronométrée qu'elle arrive ou non.** Un transport qui n'apparaîtrait dans l'histogramme de latence que sur ses succès produirait un graphe qui *s'améliore pendant une panne* — la pire forme d'un tableau de bord, celle qui rassure. Le `reason` de `push_failures_total` est un ensemble fermé (`expired`, `failed`, `unusable`) parce qu'une chaîne libre ferait qu'une alerte ne correspond plus à rien le jour où un transport reformule son erreur. Et c'est le registre de transports qui enregistre, pas le fan-out : le fan-out tient un `dyn PushDispatcher` et est délibérément incapable de dire lequel a répondu.

**`/metrics` est à côté des sondes et hors du contrat.** Il est scrapé par de l'infrastructure et non appelé par un client : pas d'entrée OpenAPI — rien de généré depuis la spec ne l'appellerait jamais —, un format texte et non du JSON, et pas d'authentification, pour la même raison que `/healthz`. La preuve que ce n'est pas un oubli est mécanique : `task recu` ne produit aucune dérive, donc la spec n'a pas bougé d'un octet. L'exposition est réelle et bornée exprès — le document porte des **compteurs et des identifiants de canaux**, jamais une notification, un utilisateur, un device ou un endpoint, et aucun label n'est non borné. Un ingress qui le publierait est une erreur de déploiement, la même que publier `/readyz`.

**Ce qu'un opérateur découvre à la première scrape**, écrit dans la page d'ops parce que ça se découvre sinon en écrivant une alerte qui ne se déclenche pas : une famille *labellisée* n'a aucune série tant que rien n'a été enregistré. Un replica fraîchement démarré rend `sync_backlog_size` — sans labels, donc une série depuis le début — et **rien d'autre**. C'est le modèle de `prometheus-client` et pas une décision d'ici ; ce qui rend une alerte robuste à ça est `absent()`, un `or vector(0)` ou une fenêtre `for:`, et un test le verrouille pour que ce soit une propriété connue et non une surprise.

**Deux tests pour deux lignes de câblage.** Le registre est testé là où il vit, mais ce qui casse dans `/metrics` est le câblage, et il casse en silence : un `app_data` dont le type ne correspond pas à ce que le handler extrait est un `500` au moment de la scrape et rien du tout à la compilation. Le test sert donc le registre dans lequel *ce* processus a compté, vérifie le content-type avant le corps (un scrapeur lit l'en-tête en premier), et un second vérifie qu'une instance sans exporteur rend un document **vide** plutôt qu'une erreur — « cette instance ne mesure rien » est une réponse de configuration, et un `500` là réveillerait quelqu'un pour un build qui se comporte exactement comme configuré.

**Vérifié en le lançant**, contre un vrai Postgres, le mock OIDC et un jeton d'alice obtenu par le vrai flow PKCE : une scrape du replica fraîchement démarré ne rend que `sync_backlog_size`, avec le content-type OpenMetrics — c'est la propriété « une famille labellisée n'existe pas encore » vue sur le binaire et pas seulement dans un test ; un `POST /notifications` fait apparaître `notifications_published_total{channel="cluster-ovh-1"} 1`, et **le rejouer avec la même `Idempotency-Key` rend 200 sans incrémenter** ; la boucle de drain tourne toutes les 5 s en lisant la profondeur de DLQ sans un seul avertissement, ce qui est le chemin de D15 exécuté pour de vrai. Et la jauge de connexions a démenti son propre commentaire, ci-dessus.

Portes au vert à la fin de M4ter : `task lint:check`, `task test` (**661** Rust exécutés + 59 e2e sautés, soit **720** contre 710 à la fin de M4bis, + 103 web), `task test:critical` (**101**, inchangé — l'exporteur n'entre pas dans la suite bloquante), `task test:e2e` (**59**, contre un vrai Postgres — 59 exécutés, zéro sauté), `task audit` (cargo + deny + pnpm + trivy — `prometheus-client` passe les licences sans exception à ajouter), et `task recu` idempotent, sans dérive. Couverture domaine+app : **95,8 %** pour un plancher à 90 %, mesurée par le gate lui-même (D10).

**Ce que M4ter ne prend pas** : la puce « Traces » de §13 — `tracing-opentelemetry`, export OTLP, propagation W3C `traceparent` en entrée. Rien de tout ça n'est câblé : `tracing` est utilisé pour des **événements** structurés, il n'y a aucun `#[instrument]` sur un use case, aucune dépendance OpenTelemetry dans le workspace, et rien dans ce build ne parle à un collecteur. Le dire ici plutôt que laisser « §13 » cocher une case entière : c'est D16 — soldée par M4quater, ci-dessous.

**Découpage de M4quater** :

| # | Livrable | État |
|---|---|---|
| M4quater.1 | `[telemetry]` + le module `telemetry` — subscriber en couches, exporteur OTLP/HTTP, flush à l'arrêt | ✅ écrit |
| M4quater.2 | Les quatre spans : `http`, `publish`/`upsert`, `fanout`, `push` | ✅ écrit |
| M4quater.3 | `traceparent` accepté en entrée (middleware), vérifié contre un collecteur | ✅ écrit |
| M4quater.4 | Page `ops/observability` — section Traces, et `config/dev.toml` | ✅ écrit |

**Éteint sauf si on le demande.** Pas de section `[telemetry]` : pas d'exporteur, pas de processeur de spans, pas de thread — le processus journalise exactement comme avant. Un défaut sur `localhost:4318` aurait fait réessayer un export à toutes les instances qui n'ont pas de collecteur, indéfiniment, et l'endroit où l'on découvre ça est la facture réseau d'un cluster. C'est le même refus que `[attachments]` absent : une dépendance qu'on n'a pas déclarée ne doit rien tenter.

**Les spans sont du `tracing` nu ; l'exporteur est dans la racine de composition.** `notify-app` porte des `#[instrument]` et ignore si quelqu'un écoute — exactement la règle du port `Metrics` de M4ter, vue sous un autre angle : ce qui est émis appartient au code, où ça part appartient à la composition. Le corollaire pratique est que la suite de tests in-memory ne porte aucun handle de télémétrie.

**Quatre spans et pas quarante.** `http` (méthode + **pattern de route**, pas le chemin — sinon la liste des opérations du collecteur est aussi longue que la table des notifications), `publish`/`upsert`, `fanout`, `push` nommé par le transport qui a répondu. Les attributs portent le canal, l'id de notification et la route ; **jamais** un titre, un corps, un utilisateur ou un endpoint push — §13 dit que les logs ne portent pas de charge utile, et il n'y a aucune raison que les spans soient plus bavards que les logs.

**Le `traceparent` entrant est cru, et c'est délibéré.** C'est un identifiant de corrélation, pas une preuve : au pire, un en-tête forgé rattache une requête à une trace qui n'est pas la sienne, dans un système où l'émetteur peut déjà écrire en publiant. Le refuser coûterait précisément ce pour quoi §13 le veut — une CI qui relie sa propre trace à la notification qu'elle publie — et n'achèterait rien.

**Échantillonnage parent-based**, décidé ici et pas au collecteur : un span jamais enregistré ne coûte rien à exporter, un span enregistré pour être jeté en aval a déjà été payé. Les enfants suivent leur racine, sinon on obtient des traces trouées au milieu, plus difficiles à lire que pas de trace du tout.

**Vérifié contre un collecteur**, et c'est ce qui a corrigé le choix de client HTTP. Écrit d'abord avec le client `reqwest` **asynchrone**, l'export a paniqué au premier envoi : le processeur de batch tourne sur son propre thread, ce thread n'est pas un runtime Tokio, et « there is no reactor running » est ce qu'on obtient — un panic dans un thread de télémétrie, donc un serveur qui a l'air d'aller bien et qui n'exporte rien. Le client **bloquant** est le bon sur un thread dédié, et c'est aussi le défaut du crate ; la raison est écrite dans le manifeste plutôt que redécouverte.

Le reste s'est vérifié du premier coup, contre un collecteur écrit pour l'occasion (trente lignes qui acceptent un POST et regardent les octets) : `service.name = weebo-notify` dans la ressource, les spans `http` → `publish` → `fanout` avec leurs attributs, `route = /api/v1/notifications` — donc `match_pattern()` **est** renseigné dans un middleware d'application, ce qu'un test verrouille désormais parce que la réponse inverse aurait donné `unmatched` sur toutes les requêtes du déploiement. Et les deux moitiés de la propagation : avec `traceparent: 00-4bf92f35…-01`, l'id de trace du caller est **dans la charge utile exportée** ; sans l'en-tête, il n'y est pas. Un `SIGTERM` juste après une publication a bien produit un troisième export, ce qui est le flush de l'arrêt.

Portes au vert à la fin de M4quater : `task lint:check`, `task test`, `task test:critical`, `task test:e2e`, `task audit` — les quatre crates OpenTelemetry passent licences et CVE sans exception à ajouter — et `task recu` idempotent : `/metrics` n'a pas d'entrée OpenAPI et les traces n'en ont pas davantage, donc la spec ne bouge pas.

**Ce que M4quater ne prend pas** : la propagation *sortante*. Un webhook de sink part sans `traceparent`, donc la trace s'arrête à la frontière de ce produit. Ce n'est pas un oubli — le consommateur d'un webhook n'est pas dans le même système de traces, et l'en-tête ne servirait qu'à ceux qui ont branché le leur —, mais c'est la première chose à ajouter le jour où quelqu'un le demande.

**M4quinquies en détail.** Une porte que trois documents promettaient et qu'aucun déploiement n'avait.

**Le constat d'abord**, parce qu'il est plus intéressant que le correctif : §10.3 demande une « CSP stricte sans `unsafe-inline` », `lib/markdown.ts` l'appelle la quatrième porte après `html: false`, DOMPurify et le contrôle de schéma, et ADR-0016 en fait « l'autre moitié » de son compromis sur les jetons. Les trois disaient qu'elle appartenait au serveur qui sert le bundle. **Ce serveur n'existe pas dans ce dépôt** : pas de Containerfile, pas de manifeste d'ingress, aucune configuration de serveur statique. La porte était donc une phrase dans trois documents et un en-tête dans zéro déploiement — exactement la forme de dette que §14.4 existe pour attraper, et elle n'y était pas.

**La politique voyage donc avec le bundle** (ADR-0019) : un plugin Vite la calcule et la stampe dans `index.html`. Un `<meta http-equiv>` est appliqué par le navigateur exactement comme l'en-tête pour toutes les directives qu'il porte, et il ne peut pas être oublié par qui écrit l'ingress. Elle part de `default-src 'none'` et ne rajoute que ce dont l'application a besoin — commencer permissif et soustraire est la façon dont une politique finit par autoriser quelque chose que personne ne sait expliquer.

**Le hash n'est jamais écrit à la main.** La page porte un script inline (le thème appliqué avant le premier paint, §10.2) ; son empreinte est lue **sur la page servie**, pas recopiée dans une constante. Un hash collé est à une édition d'espace blanc d'une page blanche — et une page blanche est la façon dont une erreur de CSP se manifeste. Le plugin revérifie ensuite le fichier réellement écrit : si un minifieur HTML ajouté plus tard réécrivait le script après le calcul, le build échoue au lieu de produire ce bundle-là.

**Deux directives ne peuvent pas voyager ainsi** — `frame-ancestors` et `report-to` sont ignorées dans un `<meta>` — et elles restent dues par le déploiement. C'est D17, ouverte plutôt que sous-entendue, et écrite aussi dans la page de durcissement où un opérateur la lira.

**`style-src` garde `'unsafe-inline'`**, et c'est le seul écart avec la lettre de §10.3. reka-ui positionne ses popovers en écrivant des attributs `style`, que CSP compte comme du style inline et qu'on ne peut pas hasher — ils changent avec le curseur. Une injection de style n'exfiltre pas un jeton comme une injection de script ; c'est la seule directive où le prix vaut la peine. Le dire ici plutôt que laisser croire que §10.3 est honoré à la virgule près.

**Ce qui est vérifié, et ce qui ne l'est pas.** Vérifié : le contenu de la politique (suite de tests — `default-src 'none'`, pas d'`unsafe-inline` dans `script-src`, `connect-src` limité à l'API et au fournisseur), le nombre de scripts inline de l'`index.html` réel — un second script ajouté sans y penser fait échouer un test plutôt qu'une page —, et le hash servi comparé octet à octet au script servi, sur un `pnpm build` lancé pour de bon. **Non vérifié : l'application par un navigateur.** Il n'y en a aucun dans cet environnement, jsdom n'applique pas les CSP, et écrire « politique testée » serait le tick vert que D8 interdit.

**Le build de production entre dans la CI**, parce qu'une politique stampée par un build que rien ne lance est une politique dont personne ne sait si elle sort. `task build:web:smoke` tourne dans le job `check` avec les valeurs de `web/.env.example` : son objet est le pipeline, pas l'artefact, et il le dit en finissant par « ce bundle n'est pas déployable ». Le vrai build est `task build:web`, qui **refuse** de construire sans les trois variables — un bundle bâti contre la mauvaise origine se charge puis ne joint pas son API, ce qui est un échec pire que pas de build du tout.

**Ce que M4quinquies ne prend pas** : `pnpm dev` tourne sans politique — le HMR de Vite a besoin d'un websocket et d'injections que la politique livrée ne doit pas autoriser, donc une politique relâchée pour le dev serait une autre politique testée à la place de la vraie. La violation introduite en développant se voit au `build`/`preview`, ou au job `check`, pas à la frappe.

**M4sexies en détail.** Une route, et surtout la méthode qui l'a trouvée.

**Le diff, pas la relecture.** §8 liste 45 endpoints ; `openapi.json` en sert 49. Comparer les deux listes — en normalisant les noms de paramètres de chemin, `{id}` contre `{channel}` — laisse **une** ligne : `GET /api/v1/channels/{id}`, « détail : membres, TTL, sinks, badge managed », listée depuis M1 et servie par aucun build. C'est exactement la forme de D14, trouvée exactement de la même façon, et la leçon est que ce diff se fait en dix secondes et aurait dû être une porte depuis le début.

**Le contournement existait, et c'est pourquoi personne ne l'avait vu** : `GET /channels` rend des `Channel` complets, pas des résumés — un client voulant un canal pouvait donc paginer jusqu'à le trouver. Ça marche jusqu'à la page où ça ne marche plus.

**`Subscribe` et pas `Manage`.** C'est la lecture que `ListChannels` accorde déjà, adressée par id au lieu de par page ; demander `Manage` voudrait dire qu'un membre ne peut pas ouvrir un canal dont il reçoit les notifications. Le use case fait trois lignes parce que `Authorizer::authorize` porte déjà la règle qui compte : **un canal qu'on ne peut pas voir et un canal qui n'existe pas rendent la même réponse**, sinon la route devient la façon la moins chère d'énumérer une instance. Un test le tient sur cette porte-ci plutôt que de faire confiance à l'héritage.

**Ce que la charge utile ne porte pas** : les sinks, que §8 nomme dans la description du détail. Ils ne sont pas sur `Channel` — ils viennent de `[[channels.managed.sinks]]`, c'est-à-dire de la configuration —, et les exposer ici publierait des destinations (une URL de webhook, une adresse de mail) à tout membre du canal, quand ADR-0018 tient à ce que la file elle-même ne porte jamais leur secret et que §10.4bis refuse déjà de montrer l'événement en souffrance. La route rend donc ce que rend `GET /channels` : id, nom, origine, policy, membres, bindings de groupe. **§8 promet une ligne de plus que ce build sert**, et le dire ici vaut mieux qu'ajouter un champ dont on n'a pas décidé la forme.

**M4septies en détail.** Le même diff, sur les deux autres surfaces que ce document décrit ligne à ligne.

**§9 contre l'arbre clap.** Quatre écarts, dont deux corrigés ici :

- **`notify device enroll` n'existait pas.** §9.1 et §9.2 écrivent la commande avec deux `l`, le crate épelle la variante avec un seul, et clap dérive donc `enrol`. La ligne copiée depuis le document répond « unrecognized subcommand » — la plus petite promesse fausse possible, et celle qui tombe dans les cinq premières minutes de quelqu'un. Corrigé par un **alias** plutôt qu'un renommage : les deux orthographes marchent, `--help` en montre une, et le document dit maintenant laquelle ;
- **le body sur stdin demandait déjà `-`**, quand §9.1 écrivait « (`-` ou pipe) » et donnait un exemple sans `-`. Ici c'est le document qui avait tort et le code qui avait raison : une CLI qui lit stdin sans qu'on lui dise est une CLI qui se bloque un jour dans un script. La phrase est réécrite plutôt que le comportement ;
- **`--notify-send` n'existe pas** (D19), et §7.4 s'appuie dessus — deux sections qui se citent l'une l'autre à propos de rien ;
- **le mode `:` n'a ni complétion ni historique** (D20), quand c'est précisément ce qui devait en faire un filet de découvrabilité.

**Trois tests, parce qu'un diff fait à la main ne se refait pas.** `Cli::command().debug_assert()` — la vérification que clap offre gratuitement et que ce crate n'appelait pas ; l'alias `enrol`/`enroll` sur les deux orthographes ; et `--output` cherché sur **chaque** sous-commande, parce que « §9.1 promet `--output json|table` partout » est une propriété qui se découvre dans un pipeline quand elle tient pour huit commandes sur neuf.

**§11 contre `config.rs` : rien à signaler**, et c'est un résultat. Chaque clé du bloc d'exemple existe comme champ typé, sauf `[push.apns]` et `[push.unifiedpush]`, qui sont parsés en `serde_json::Value` opaque et **signalés au démarrage** par `Config::unhonoured()` comme arrivant avec M6 et M7. Un manque annoncé au boot n'est pas un manque : c'est la règle que ce projet applique depuis M1, et c'est ce que le diff vérifie.

**M4octies en détail.** Ce que les deux diffs précédents ont appris, transformé en portes — plus le seul scénario de §3.4 qui manquait pour de bon.

**T23 existait sur le papier et nulle part ailleurs.** §3.4 le décrit comme « test généré qui itère le schéma complet et vérifie que chaque clé est surchargeable par `WNK_…` », et rien de tel n'était écrit — la seule vérification était celle qu'on venait d'ajouter pour `[telemetry]`, c'est-à-dire une clé sur vingt-quatre. Il est là maintenant, et « généré » veut dire ce qu'il dit : le schéma est **lu sur la configuration sérialisée** plutôt que recopié, donc un champ ajouté demain est parcouru demain sans que ce test bouge.

Trois choses se sont décidées en l'écrivant, toutes les trois en le faisant échouer d'abord :

- **il se lance depuis `config/dev.toml`, pas depuis les défauts.** Une configuration par défaut ne sérialise que **onze** scalaires — les sections optionnelles sont `null`, les vecteurs sont vides —, donc la première version passait en couvrant presque rien. Le fichier commité remplit `[attachments]`, `[push.webpush]`, `[sinks.smtp]` : vingt-quatre clés, et c'est aussi la forme qu'un opérateur surcharge réellement, une variable à la fois ;
- **deux issues sont acceptables par clé, pour la même raison** : ou bien la valeur est prise, ou bien elle est refusée **en se nommant** (un `bind` qui ne parse pas comme socket le dit). Les deux prouvent que la variable a *atteint* le champ. Ce que le test refuse est la troisième issue, celle qui ne se voit jamais : la variable ignorée en silence ;
- **un compteur garde le garde-fou** : si toutes les clés finissaient dans la branche « refusée en se nommant », le test vérifierait des messages d'erreur et rien d'autre. Il exige donc que la majorité soit réellement prise.

**Vérifié en le cassant** : en retirant le `merge(Env::prefixed(...))` de `Config::load`, le test échoue à la première clé (`WNK_ATTACHMENTS__ACCESS_KEY`). Un test de configuration qui passerait sans que la configuration marche est précisément ce que D8 interdit.

**Le contrôle de routes ne regardait que les chemins.** `the_openapi_document_covers_every_route_that_is_registered` listait `/api/v1/channels/{channel}` et passait — parce que `PATCH` et `DELETE` enregistrent ce chemin. C'est **pourquoi D18 a survécu quatre milestones** sous un test qui avait l'air de couvrir la question. Le nouveau contrôle porte les 45 couples **(méthode, chemin)** de §8, et il a lui aussi été vérifié en le cassant : une ligne inventée (`PUT /search`) le fait échouer en nommant ce que le chemin sert vraiment.

**§2.3 et §7 diffés, propres.** Les cinq types d'action et leurs champs, les limites (5 actions, 40 caractères de label, 10 options de `choice`), `silence` de `scope: group` qui **exige** une durée, les schémas d'URL refusés à l'ingestion, le strip ANSI sur tous les champs texte : tout est là, avec ses tests. Côté §7.4, le modèle DND porte `quiet_hours` avec sa timezone, `until`, `scope`, `exceptions` et `urgent_bypasses`, et `deferred_by_dnd` voyage dans le sync. Rien à signaler — mais cette phrase ne valait d'être écrite qu'une fois la comparaison faite.

**M4nonies en détail.** T28, et la différence entre « relâché » et « tué ».

**Cinq tests existaient et couvraient la moitié facile.** L'élection par fichier de lock (§9.3bis) était écrite et testée : la première frontale gagne, la seconde s'entend dire non sans attendre, deux profils ne s'élisent pas l'un l'autre, le lock est à côté du store. Et « le détenteur s'en va » était testé **en relâchant un bail**, c'est-à-dire en exécutant son `Drop`.

**Or T28 dit `kill -9`, et un `kill -9` n'exécute rien.** La libération vient alors du système d'exploitation, et c'est exactement pour ça que §9.3bis a choisi un lock advisory plutôt qu'un drapeau en base : un drapeau dirait encore « tenu » après un crash, et l'app en tray ne récupérerait jamais le flux sans qu'un humain supprime un fichier à la main. C'est la propriété qui distingue les deux conceptions, et elle n'était vérifiée nulle part.

Le test la vérifie maintenant pour de vrai : le binaire de test **se relance lui-même** avec une variable qui lui dit de prendre le bail et d'attendre, le parent lit sur son stdout le marqueur qui dit « je le tiens » — plutôt que de dormir un intervalle deviné —, constate qu'il ne peut pas l'obtenir, envoie `SIGKILL`, et l'obtient immédiatement après. Aucun délai de grâce, aucune boucle de retry : le descripteur part avec le processus.

Une chose s'est apprise en le faisant échouer : libtest écrit `test <nom> ... ` **sans retour à la ligne** avant de laisser le test s'exprimer, donc le marqueur de l'enfant atterrit au milieu de cette ligne. Le parent cherche donc un jeton *dans* une ligne au lieu de comparer une ligne entière — sinon il lit jusqu'au bout du sommeil de l'enfant et conclut, à tort, que l'enfant n'a jamais pris le lock.

**L'autre moitié de T28** — « suppression du store → re-sync complet, état final identique » — repose sur une phrase de §9.3bis (« le store n'est qu'un cache ») et sur un seul fait : le curseur vit dans le fichier. Le test supprime le store, ses fichiers WAL compris, rouvre, et exige un curseur **absent**. Ce qu'il exclut est l'inverse du bug qu'on imagine : non pas la perte, mais un store qui revient **avec** un curseur qu'il ne peut pas justifier — copie périmée, valeur par défaut, ligne à moitié écrite — et qui saute donc tout ce qui a été publié depuis. L'id de device part avec, ce qui est la bonne réponse : la frontale se ré-enrôle au lieu de poser des receipts au nom d'un device que le serveur a peut-être révoqué.

**M4decies en détail.** T24, et la base qu'il fallait créer pour l'écrire.

**Le scénario dit « boot sur base vide »**, et la suite e2e n'en a pas : sa base est migrée avant le premier test, par construction. Les migrations tournaient donc à chaque exécution sans que personne vérifie ce qu'elles produisent — un `migrate()` qui échouerait à moitié se serait vu, un `migrate()` qui **rejoue** ne se serait pas vu du tout.

Le test crée donc la sienne (`CREATE DATABASE`, un nom par processus et par appel), y fait deux boots, et la supprime — y compris quand une assertion échoue, ce qui est la raison pour laquelle le corps vit dans une fonction qui rend un `Result` au lieu de paniquer sur place : une base d'essai abandonnée sur la machine de quelqu'un est un déchet qu'on ne remarque qu'au troisième.

**Ce qui est vérifié tient en deux lignes**, et la seconde est celle qui compte : après le premier boot, les tables de §4 existent (`channels`, `notifications`, `events`, `devices`, `sink_queue`) ; après le second, `_sqlx_migrations` est **identique, `installed_on` compris**. C'est cette colonne qui trahit un rejeu : une migration réappliquée porterait un nouvel horodatage même à version égale, là où comparer les seules versions laisserait passer exactement le bug recherché.

Le rôle de test a le droit de créer une base ici ; sans lui — ou sans base du tout — le test se saute proprement, comme le reste de la suite.

**M4undecies en détail.** Les deux moitiés que D20 nommait, et pourquoi elles étaient écrivables ici.

**La complétion vient de l'arbre, pas d'une liste.** §9.2 dit « complétion contextuelle depuis l'arbre clap », et l'insistance est sur *depuis l'arbre* : une liste écrite dans le TUI serait une seconde copie de la surface de commandes, et le jour où les deux divergent c'est la fausse qu'on lit. `suggestions()` descend donc dans `Cli::command()` — sous-commandes du niveau atteint, drapeaux longs du niveau atteint — et une sous-commande ajoutée au shell apparaît dans le TUI sans une ligne de TUI. C'est la règle que le parseur suivait déjà (D4), appliquée à l'autre moitié du mode `:`.

Trois décisions se lisent mal sans leur raison :

- **la ligne est découpée sur les espaces, pas avec les règles du shell**, contrairement à l'exécution. Une ligne en cours de frappe est fréquemment *invalide* — un guillemet non fermé est l'état normal de quelque chose qu'on est en train d'écrire — et un découpage qui échoue là ne complète rien ;
- **les mots inconnus ne cassent pas le contexte** : `:update -c ci-infra --sta⇥` complète encore contre `update`, parce que la descente ignore ce qui n'est pas une sous-commande au lieu de s'arrêter au premier drapeau ;
- **les drapeaux n'apparaissent qu'après un `-`**, et `help` jamais. Une invite qui répond vingt drapeaux à une tabulation nue est une invite qu'on ne tabule pas deux fois.

Plusieurs candidats complètent **jusqu'où ils sont d'accord** (`--st⇥` → `--stat`, et pas un choix arbitraire entre `--state` et `--status`), un seul candidat ajoute l'espace qui ouvre le mot suivant. C'est le comportement que tout le monde a déjà dans les doigts.

**L'historique est un fichier, à côté du store et pas dedans.** Persisté parce qu'un TUI s'ouvre et se ferme toute la journée, et qu'une mémoire qui meurt avec le processus est une mémoire vers laquelle personne ne prend le réflexe d'aller. À côté du store et non à l'intérieur : §9.3bis dit que le store est un cache qu'on peut supprimer à tout moment pour forcer un re-sync, et perdre son historique en vidant un cache serait une surprise. En `0600` sur Unix, pour la raison qu'un historique de shell l'est — une ligne `:publish` porte un canal, un id de run, un titre. Répéter la ligne précédente ne la réécrit pas ; deux profils gardent deux historiques, comme ils gardent deux stores et deux devices ; et une frontale sans répertoire de données (le `notify watch` éphémère de §9.3bis) garde quand même sa mémoire le temps qu'elle tourne.

**Ce qui rendait D20 faisable ici, sans terminal** : les deux moitiés sont testables hors écran. La complétion est une fonction pure sur l'arbre clap — neuf tests, dont la descente avec un drapeau au milieu et le préfixe commun. L'historique est du fichier — huit tests, dont les permissions et la troncature. Restent les touches, qui sont du câblage : `tab`, `↑` et `↓` doivent appartenir à la ligne de commande **tant qu'elle est ouverte** et rendre la main au reste de l'interface dès qu'elle se ferme, ce qu'un test vérifie dans les deux sens. La barre de la ligne affiche maintenant ces trois bindings : une complétion que personne ne connaît est une complétion que personne n'utilise.

**M4duodecies en détail.** La suite qui devait porter trente scénarios en portait vingt-six.

**Le profil est un filtre, et le filtre ne ment pas.** `.config/nextest.toml` sélectionne `test(critical)` : un scénario entre dans la suite bloquante en portant `critical` dans son nom, et pas autrement. Lister les tests du profil et en extraire les numéros donne donc la couverture réelle — un `cargo nextest list -P critical` de dix secondes, qu'il aurait fallu faire dès que la convention a été écrite. Le résultat : **T15, T23, T24 et T28 manquaient**.

**Trois d'entre eux étaient mes propres tests**, écrits dans les incréments précédents et nommés hors convention : ils tournaient dans `task test`, pas dans la porte à laquelle ils appartenaient. Renommés (`critical_t23_…`, `critical_t24_…`, `critical_t28_…`), ils y entrent — et ce petit ratage vaut d'être écrit, parce qu'il est exactement la forme que prend une convention non vérifiée : tout le monde la respecte tant que quelqu'un la relit.

**T15, lui, n'était appliqué nulle part**, et c'est la vraie trouvaille. §8.2 plafonne le payload à 64 KiB ; `MAX_PAYLOAD_BYTES` existait dans `limits.rs` depuis M1 et **aucun code ne le lisait** — la constante était déclarée, documentée, et morte. Chaque champ avait sa propre limite, ce qui a rendu le trou invisible : elles ne s'additionnent pas. Un body de `MAX_BODY_CHARS` **caractères** pèse jusqu'à quatre fois ce nombre en octets, une image inline en ajoute 48 KiB, et le total passe le plafond sans qu'aucun champ ne le dépasse.

Le contrôle est donc sur **l'ensemble sérialisé**, après validation des champs : c'est ce que le plafond décrit — ce qui voyage, ce qu'un transport push doit porter (§6.2), ce qu'une ligne stocke. Deux tests le tiennent des deux côtés, parce qu'un plafond ne se teste pas seulement par ce qu'il refuse : un body honnête plus une capture d'écran de 8 KiB doit passer, sinon la moitié utile de §2.3 devient inutilisable. Et au niveau HTTP, ce qui est vérifié est le **code** — `413` avec `payload.too_large` — parce qu'un publisher qui réessaie éternellement un 500 mal mappé est le mode d'échec que §8.2 existe pour éviter.

Le presign avait déjà son plafond ; il n'avait pas de test à la surface. Il en a un : 26 MiB refusé **avant** signature, parce qu'une signature accordée pour un upload que le bucket rejettera est un aller-retour et un message d'erreur incompréhensible.

`cargo nextest list -P critical` répond maintenant les trente numéros, et la suite passe de 101 à **109** tests.

**§10 contre `web/src`, pour finir le tour.** Le dernier document dont les promesses n'avaient pas été comparées au code. Le shell trois zones est là, la sidebar avec ses filtres, le panneau détail, les permaliens `/n/<id>` et `/c/<id>`, les écrans Inbox, Admin, Console et Settings. Manquent la **palette `Cmd+K`**, la **recherche** — et avec elle un endpoint testé que personne ne peut atteindre —, et l'écran **Apps** qui attend des releases. C'est D21 — puis M4terdecies l'a écrite quand même, sur demande explicite, avec la mise en garde tenue : la logique est testée, le rendu ne l'est par personne.

**M4terdecies en détail.** La palette de §10.4, et une frontière tracée à l'intérieur.

**Ce qui est vérifié et ce qui ne l'est pas, séparé par un fichier.** `lib/palette.ts` décide ce qui est offert et dans quel ordre ; `stores/palette.ts` décide ce qui est demandé au serveur ; `CommandPalette.vue` décide de ce que ça a l'air. Les deux premiers ont dix-huit tests ; le troisième n'en a aucun et n'en aura pas ici. Ce n'est pas une commodité de découpage, c'est la seule manière honnête de livrer sans navigateur : la partie dont le comportement se démontre est isolée de la partie dont l'apparence se juge.

**L'appariement est une sous-séquence, comme un « go to file ».** `cov` trouve `cluster-ovh-1`. Trois rangs, dans l'ordre qu'une personne attend : préfixe exact, début de mot, sous-séquence dispersée ; à rang égal, le plus court gagne — taper `ci` et obtenir `ci-infra-staging` avant `ci` est la petite fausseté qui fait qu'on n'utilise plus une palette. Rien de plus savant : un Levenshtein achète de la précision sur des listes que ce produit n'a pas (des dizaines de canaux, pas des milliers) et coûte un comportement que personne ne prédit depuis ses frappes.

**Le serveur est interrogé avec des ménagements, et les deux raisons sont différentes.** La requête est **débouncée** parce que §8.2 plafonne la recherche à 30/min par principal — une requête par frappe brûle ce quota en vingt secondes de frappe honnête. Et **la dernière réponse gagne** parce que sur un réseau lent la réponse à `etc` arrive après celle à `etcd` et laisse la mauvaise liste sous le curseur : un bug invisible en local et permanent ailleurs. Les deux ont leur test.

**Une recherche qui échoue ne fait pas tomber le switcher** : les canaux et les écrans s'apparient en mémoire et restent atteignables, l'erreur s'affiche sur la palette. C'est la même règle que partout ici — une dépendance absente dégrade, elle ne casse pas.

**Le vocabulaire est celui du TUI** (§9.2), et c'est écrit dans un test plutôt que dans une convention : les verbes `dnd:1h` et `seen` sont ceux du mode `:`, pour que ce qu'on apprend d'un client se devine dans l'autre.

**Ce que la livraison ne prouve pas** : que ça s'affiche correctement, que le focus revient au bon endroit, que `Esc` referme, que la liste se lit. Reka UI porte ces comportements (ADR-0004) et c'est précisément pourquoi la modale s'appuie dessus au lieu d'un gestionnaire de touches écrit ici — mais « la bibliothèque le fait » est une affirmation, pas une observation. **D21 reste en jaune** jusqu'à ce que quelqu'un l'ouvre.

**M6ter en détail.** Une demi-phrase de §8.2, en blanc depuis M4octodecies, et fermée seulement une fois la question de droits tranchée plutôt que devinée.

**Ce que M4octodecies avait refusé de faire semblant de faire.** « défauts globaux, surchargeables par canal (§11) **et par API key** » — la première moitié soldée, la seconde jamais posée, parce qu'elle butait sur une question que §8.2 et §5.2 ne tranchent pas : qui a le droit de fixer le plafond d'une clé ? Une clé créée par son propre porteur avec son propre plafond ne serait plus un plafond. Écrite en dette plutôt que devinée en code, comme D27 avant elle.

**La réponse : platform-admin, jamais le porteur.** Même refus que `SetAvatar` fait pour le porteur d'un compte de service, dans un sens différent — ici ce n'est pas *qui peut agir sur cette clé* qui est en jeu, c'est *qui a intérêt à ce que le plafond mente*. `RevokeApiKey` reste ouvert au porteur, à dessein : « ça a fuité, tue-la » doit rester accessible à la première personne qui le remarque. `SetApiKeyLimit` ne l'est délibérément pas — le porteur d'une clé bavarde est justement celui à qui ce plafond s'adresse.

**Le plafond voyage sur le `Grant`, pas re-cherché à la volée.** `Grant::ApiKey` gagne un champ `publish_per_minute`, rempli une fois à l'authentification — la clé a déjà été chargée pour construire ce `Grant`, la relire au moment de publier serait une deuxième lecture pour une réponse que la première tenait déjà. `Writer::check_quotas` (design de M4septdecies) lit `caller.grant` exactement comme il lit `channel.policy.publish_per_minute` : deux plafonds, une seule boucle, une seule question à `RateLimiter::check` chacun.

**Deux arguments de plus à `PublishNotification::new`, et la leçon de M6bis appliquée avant qu'elle ne se pose.** Le plafond du canal vit sur `Channel`, celui de la clé aurait pu vivre sur un paramètre de plus à `Writer` — sauf que `clippy::too_many_arguments` l'aurait refusé, comme il a refusé la même chose à `Fanout::new` pour `derive_thumbnail`. Ici la question ne se posait même pas : le `Grant` était déjà construit à l'authentification et déjà porté par `Caller`, donc il n'y avait rien à ajouter nulle part — la lecture du plafond est un `match` de plus dans une fonction qui existait déjà, pas un paramètre de plus dans un constructeur.

**`BIGINT`, pas `INTEGER`, et c'est écrit plutôt que découvert au premier débordement.** Le domaine porte un `u32` ; la colonne aurait pu être `INTEGER` (`i32`) et forcer chaque écriture à vérifier que la valeur y tient. Élargie à `BIGINT` à la place : la seule chose qui y est jamais écrite est un `u32` que ce code a lui-même produit, donc la question ne se pose jamais — et le test contre un vrai Postgres le prouve avec `u32::MAX - 1`, pas avec un nombre rond qui aurait réussi pour la mauvaise raison.

**Vu par l'API, dans les deux sens** : un platform-admin fixe le plafond d'une clé qu'il n'a pas créée et la réponse porte la nouvelle valeur (`200`) ; le porteur de cette même clé, avec le même corps de requête, reçoit `403` — la propriété qu'un test unitaire aurait pu affirmer sans jamais prouver que la route elle-même refuse la bonne personne.

**Ce que cet incrément ne prend pas.** D23 reste ouverte, et le reste délibérément : §8.2's autre moitié — le plafond de connexions par replica — pose une question d'exploitation qu'aucun déploiement ici n'a encore posée, et le texte qui l'ouvre le dit déjà : à trancher quand un déploiement réel aura plus d'un replica, pas avant.

**M6bis en détail.** La dette la plus ancienne de la liste, soldée en dernier parce qu'elle posait la question que D28 avait déjà posée pour l'audit — pas « comment », mais « le serveur a-t-il le droit de décoder ceci ».

**Ce que §15 interdisait, en toutes lettres.** « L'avatar est la seule surface de décodage média du serveur » — une phrase répétée dans `notify-ports::media` et dans `notify-app::attachments` depuis M2. Une vignette de row exige exactement ce que la phrase refuse : quelque chose côté serveur qui va chercher l'objet dans le bucket et le décode. L'élargir n'était pas une ligne à ajouter à une liste, c'était rouvrir une frontière de menace posée pour de bonnes raisons — d'où la décision demandée avant tout code plutôt que devinée en écrivant.

**La décision, prise dans la couche qui la porte.** Deux options se valaient sur le papier : dériver au moment du `publish`/`revise` (coût sur le chemin d'écriture, un seul déclencheur) ou paresseusement à la première lecture (chemin d'écriture intact, mais une course entre deux lecteurs simultanés et un cache à invalider). La première a été retenue — le coût est un aller-retour bucket et un décodage, une fois par notification qui montre un attachment, jamais par lecture — et elle est posée là où §13 dit déjà que la logique d'écriture vit : `Fanout`, en dernier des quatre choses qu'un write déclenche, après la diffusion temps réel et jamais avant. La livraison en direct ne doit jamais attendre une vignette que personne n'a encore demandé à voir.

**Un second décodeur, pas une seconde politique.** `notify-avatars` gagne `ImageAttachmentThumbnailer`, qui partage son pipeline avec `ImageAvatarProcessor` — même liste de formats acceptés, même défense anti-bombe de décompression sur les dimensions avant décodage complet, même ré-encodage qui efface les métadonnées. La seule différence assumée : pas de plafond en octets propre au thumbnailer, parce que sa source a déjà traversé celui de l'avatar — `MAX_ATTACHMENT_BYTES` (§8.2), imposé à la signature de l'upload avant que ces octets n'existent dans le bucket.

**Échoue en silence, et c'est la propriété qui compte.** Un GIF animé, un upload jamais arrivé, un déploiement sans `[attachments]` : les trois répondent `None`, jamais une erreur qui remonterait au publisher. `Fanout::derive_thumbnail` est *best-effort* dans les deux sens — une source absente ne dérive rien, un rejet de décodage ne dérive rien, et aucun des deux ne fait échouer le write qui vient de réussir.

**Servie par l'API, pas présignée** — la même raison que l'avatar : elle s'affiche à côté de chaque ligne visible d'une liste potentiellement virtualisée, et une signature par rendu voudrait dire un aller-retour de signature par ligne. `GET /attachments/{id}/thumbnail` répond les octets, `cache-control: immutable`, pour la même raison que l'avatar : un attachment n'est jamais remplacé en place, une révision qui change d'image référence un `AttachmentId` différent.

**Une seule réponse pour trois questions différentes**, comme `ReadAttachment` le fait déjà pour le téléchargement complet : caller sans accès à la notification, id inexistant, rien jamais dérivé — les trois rendent `notification.gone`. Distinguer le troisième des deux premiers confirmerait l'existence d'un attachment à quelqu'un qui ne peut pas lire ce qui le montre, exactement l'énumération que la route sœur refuse déjà.

**Vu, contre un vrai Postgres/RustFS et un vrai Chrome — et ça a trouvé quelque chose.** `task dev:run`/`task dev:web`, connexion PKCE réelle via le sidecar navigateur, une clé d'API créée depuis l'écran web (§10.4, « Clés d'API »), `notify publish --image` sur 360 KiB de bruit. La row a rendu un `<img>` de 128×128 — `THUMBNAIL_SIZE` pile — chargé depuis la nouvelle route : la chaîne complète, CLI → presign → `derive_thumbnail` → route → row, tourne pour de vrai. Le regarder a aussi soulevé un `409` inattendu dans la console : `notification.gone` vaut `ErrorKind::Conflict`, testé et fixé à `409` dans `problem.rs` depuis toujours — mais `GET /avatars/{id}` documentait `404` depuis M1.10 et `GET /attachments/{id}` depuis M4unvicies, et `.../thumbnail` avait recopié le même `404` en écrivant cet incrément, juste au-dessus dans ce même paragraphe. Le comportement du serveur n'avait jamais bougé ; seule l'annotation `#[utoipa::path]` mentait, et avec elle le spec committé et les deux clients générés — un client qui aurait branché sur `response.status === 404` pour l'une de ces trois routes n'aurait jamais vu son propre code s'exécuter. Les trois annotations sont corrigées (`422` pour l'avatar, `409` pour les deux attachments), `task recu` a rejoué le spec et les clients.

**Vu où le code, pas la doc, décide** : `Fanout::new` a sept paramètres depuis toujours, et clippy le rappelle à 8. Plutôt que d'ajouter deux arguments à `PublishNotification::new` et `UpsertNotification::new` — dupliqués, puisque les deux construisent le même `Writer` — la dérivation rejoint `Fanout` par un `with_thumbnails(...)` qui suit exactement la forme de `with_sinks(...)` : optionnel, absent par défaut, une paire tenue ensemble parce qu'elle n'a de sens que câblée en un bloc. Aucun appelant de `PublishNotification`/`UpsertNotification` n'a bougé.

**M6 en détail.** Le premier incrément qui re-chiffre, et la moitié qui s'écrit sans Android.

**Ce que le device model attendait depuis M1.** `notify-domain::device` refuse d'enrôler un device sans clé publique pour un transport relayé — l'invariant écrit avant que le transport existe, « pour qu'aucun device ne soit enrôlé qui devrait plus tard être ré-enrôlé ». `notify-push-unifiedpush` était jusqu'ici le fichier d'un seul commentaire, `//! UnifiedPush transport (design §7.1). Milestone M6.` ; c'est la première fois que ce commentaire devient du code.

**Pas `crypto_box_seal`, et c'est écrit dans le module plutôt que supposé équivalent.** `sealed_box` fait ECDH X25519 éphémère puis `HKDF-SHA256` puis `XChaCha20-Poly1305` — trois étapes séparées, là où `crypto_box_seal` de libsodium dérive son nonce des deux clés publiques parce que `crypto_box` fait déjà passer le point ECDH par HSalsa20 comme dérivation de clé. `x25519-dalek` rend le point ECDH **brut**, donc la boîte ici lui ajoute ce que NaCl fournit tout seul : HKDF pour que la clé soit indistinguable de l'aléatoire, un nonce aléatoire de 192 bits qui voyage avec le ciphertext plutôt que d'être dérivé — 192 bits plutôt que les 96 bits de `ChaCha20Poly1305` nu, pour que la collision reste négligeable à n'importe quel volume que ce produit poussera un jour. Une clé faible (le point identité, valide en hex et non-contributoire en Diffie-Hellman) est refusée plutôt que scellée — T18 la tient, avec le clair qui ne ressort ni de la même clé ni deux fois de la même façon.

**Le fil reste celui que §6.3 décrit** : `{channel_id, notification_id, ciphertext_device}`, les identifiants en clair et le contenu seul scellé — un distributeur route ou coalesce sans jamais tenir de clé privée, il n'en a aucune. Un budget de 2 800 octets écrit plutôt que cité : UnifiedPush ne garantit pas de taille minimale acceptée par un distributeur, contrairement à Web Push (RFC 8291), donc c'est une marge délibérément conservatrice — 72 octets de scellement, un tiers de plus en base64, une grosse centaine d'octets d'enveloppe JSON — et non un chiffre qu'un vrai distributeur aurait fait citer.

**Câblé comme Web Push, en plus simple.** `push_transports()` (`wiring.rs`) enregistre le transport si `[push.unifiedpush]` est présent ; la seule façon dont `UnifiedPush::new()` échoue est le client HTTP, parce que la section ne porte **aucune** clé — contrairement à VAPID, rien ici n'identifie le serveur à un relais, la destination et le secret sont tous les deux du device. `Config::unhonoured()` ne nomme plus `[push.unifiedpush]` parmi ce que ce build ignore ; `[push.apns]` y reste seul, pour M7.

**Ce que cet incrément a refusé de prendre, par écrit dans le module lui-même.** §7.1bis dit deux fois que « le serveur expose lui-même le endpoint push UnifiedPush » — un token opaque par device, une porte publique de plus soumise aux quotas de §8.2 — pour le cas où le distributeur est **embarqué dans l'app** plutôt que système. Aucune route de §8 ne le liste et aucune n'est ajoutée ici : `notify-push-unifiedpush` est la moitié **sortante** seulement, celle qui fait ce que `notify-push-webpush` fait déjà pour son transport. L'entrée n'a de sens que pour un flux dont l'autre bout est Kotlin/Android — écrite plutôt que devinée, en **D32**.

**Ce qui n'est pas prouvé, et ne peut pas l'être ici.** `open()` — le déchiffrement — n'existe que sous `#[cfg(test)]` : en production il tourne côté device, hors de ce dépôt, et son seul rôle ici est de démontrer que ce que `seal()` produit s'ouvre avec la bonne clé et avec aucune autre. Aucun distributeur UnifiedPush réel, système ou embarqué, n'a reçu un envoi de ce transport — à la différence de Web Push (M3.5), vérifié contre un vrai navigateur, cet incrément reste **écrit, jamais vu**, et le restera tant qu'aucun device Android n'existe pour recevoir.

**M4duodetricies bis en détail.** Un calcul écrit avant que trois menus n'en écrivent trois versions.

**La forme est celle de D22, un cran plus haut.** §10bis.3ter donne au menu tray trois choix rapides — `1h`, `jusqu'à demain 8h`, `désactiver` — et §9.2 les donne au mode `:` du TUI. Chacun doit transformer un libellé en **instant** avant d'écrire la préférence, parce que §7.4 stocke `until` et pas « une heure à partir du moment où tu lis ceci ». D22 était trois copies de la *règle* DND ; ceci aurait été trois copies de son arithmétique.

**Écrit dans `notify-client-core` et pas dans `notify-domain`**, après un premier jet dans le mauvais crate. Le serveur ne pose jamais cette question : un preset est une affordance de client, et les clients dépendent de ce crate plutôt que du domaine du serveur — c'est d'ailleurs pourquoi `notify-cli` ne compilait pas contre le premier jet. Bonus non prévu : côté client tout est déjà en `chrono`, donc il ne reste aucune conversion `time` ↔ `chrono` à faire.

**« Demain 8h » n'est pas demain.** C'est le **prochain** 8h sur l'horloge de la préférence. Lu littéralement à 01:00, le libellé silencerait trente et une heures — toute la journée de travail qui vient, c'est-à-dire l'inverse de ce que veut quelqu'un qui l'active à une heure du matin. Le preset veut dire « jusqu'à ce que je sois debout », donc il résout au prochain 8h et c'est le libellé qui est un peu lâche. Écrit dans la fonction, testé dans les deux sens.

**Et la zone est celle de la préférence**, jamais celle de la machine : §7.4 est explicite, quelqu'un en déplacement garde ses propres matins. Un test tient le point en résolvant le même instant à Paris et à Tokyo et en exigeant deux réponses. Un autre tient le changement d'heure : le piège est de prendre le décalage *maintenant* — encore l'hiver — et de le réutiliser pour une cible qui tombe après le saut, ce qui poserait l'échéance une heure trop tard.

**Un appelant aujourd'hui, pas un pour plus tard.** `notify dnd on --until-morning` : une option utile en soi, et la même que le tray offrira. C'est la leçon de D27 appliquée d'avance — un port implémenté deux fois et appelé par personne est ce qu'on venait d'aller nettoyer, et écrire un preset pour un tray qui n'existe pas aurait été la même chose. Au passage, `now_plus` et lui partagent maintenant un `now_utc()` : la raison pour laquelle ce dépôt n'appelle pas `chrono::Utc::now` — chrono est pris sans sa feature `clock` — était écrite dans un commentaire au milieu d'une fonction, elle est maintenant sur la fonction qui la porte.

**M4untricies en détail.** La couche « feeling natif » vit dans le front partagé, donc elle se teste et se regarde sans coquille.

**Le découpage continue de payer.** §10bis.3bis est la plus grosse section de M5 — huit puces — et on la lisait comme du travail Tauri. Elle ne l'est presque pas : la sélection, les curseurs, la typographie, les scrollbars et l'anneau de focus sont des règles CSS dans le **front partagé**, celui que §10bis.3 décrit comme « l'empaquetage du front existant ». Ce qui manquait n'était pas une fenêtre, c'était le moyen de savoir qu'on est dedans.

**Un drapeau au runtime, et un seul build.** `lib/desktop.ts` demande à `globalThis` si Tauri l'a injecté et pose `is-desktop` sur la racine **avant que l'app soit montée** — une frame de style navigateur est exactement l'artefact que cette section interdit. Deux builds auraient été deux artefacts à tenir en phase et une classe de bugs qui n'existe que dans l'un des deux. Les deux globals sont testés, `__TAURI__` et `__TAURI_INTERNALS__`, parce que Tauri v2 a renommé l'objet et qu'un shell épinglé sur l'un ou l'autre doit être reconnu : se tromper ici donne une app de bureau qui ressemble silencieusement à un site, c'est-à-dire précisément l'échec de revue que §10bis.3bis nomme.

**Et rien de comportemental ne s'y accroche**, ce qui est écrit dans le fichier plutôt que sous-entendu : le drapeau vient de l'embarqueur, donc une branche qui déciderait d'une *capacité* serait une branche qu'un embarqueur peut franchir en mentant. Présentation seulement, et le build web reste le strict.

**L'exception qui compte.** `user-select: none` sur toute l'app est ce qui fait qu'elle ne se comporte pas comme une page — mais le corps d'une notification est du **contenu**, et quelqu'un veut en copier l'erreur. `Markdown.vue` porte `data-selectable`, marqué là parce que c'est le seul endroit qui connaît la différence. Vérifié dans un vrai navigateur : la chrome rend `none`, le corps rend `text`.

**Le `eval` de la coquille est parti, et il cachait un vrai bug.** Le premier brouillon injectait un `pushState` dans la webview puis émettait un `popstate` de synthèse. Ça marche, et ça passe **à côté du routeur Vue** : tous ses gardes sont sautés. Or `/n/<id>` ouvert déconnecté est censé passer par le garde de login et revenir après (`router/index.ts`) — la version `eval` aurait déposé quelqu'un sur un panneau de détail vide. La coquille émet maintenant un événement, le front écoute et navigue avec son propre routeur.

**Deux portes plutôt qu'une, pour la même valeur.** Le parseur Rust refuse ce que l'OS lui donne et ne laisse passer que deux formes ; `lib/deeplink.ts` revalide côté front. Pas par superstition : cette fonction est atteignable par tout ce qui peut émettre un événement Tauri dans cette webview, et « l'autre côté a vérifié » cesse d'être vrai dès qu'il y a un second autre côté. Un chemin protocol-relatif (`//host/n/x`) passe n'importe quel test qui se contente de « ça commence par un slash ».

**`@tauri-apps/api` est importé dynamiquement**, et c'est vérifié plutôt qu'espéré : le build sort `event-*.js` en chunk séparé de 1,1 kio à côté d'un bundle de 576 kio, donc un navigateur n'en télécharge pas un octet. C'est aussi pourquoi il est en `devDependency`.

**M4tricies en détail.** M5 commencé par l'autre bout, parce que le bout habituel est bloqué.

**La contrainte, et le découpage qu'elle impose.** D13 tient toujours : la pile GTK/WebKit n'est pas dans l'image, elle ne s'installe pas sans root, et `task tauri:doctor` le dit en une commande. Ce qui a changé est la question posée — non plus « peut-on livrer M5 » mais « quelle part de M5 ne lie rien ». La réponse est plus grande qu'attendu, et c'est le seul résultat intéressant de cet incrément.

**Deux modules purs, testés partout.** `notify_client_core::tray` porte la machine à états de §10bis.3ter, et `::deeplink` le parseur de `weebo-notify://n/<id>`. Ni l'un ni l'autre n'a besoin d'une fenêtre ; tous les deux tournent sous `task test` sur cette machine, aujourd'hui. Ils sont dans `notify-client-core` et pas dans la coquille pour la raison que §10bis.3ter énonce lui-même — le compteur du tray, celui du badge dock et celui de la sidebar viennent du **même état**, « trois rendus, une seule vérité » — et une machine à états écrite dans la coquille aurait été une quatrième vérité, la seule que personne ne pourrait tester sans session de bureau.

**Une précédence que le design ne tranche pas.** §10bis.3ter donne quatre états d'icône et jamais leur ordre, alors qu'ils sont simultanés en pratique : on peut être déconnecté, en DND et avoir des non-lus au même instant. L'ordre retenu est **déconnecté > DND > non-lus**, et le raisonnement est ce à quoi l'icône sert : chaque variante non neutre répond à « pourquoi est-ce silencieux ? », et les réponses ne se valent pas. Être déconnecté est une **panne** que personne n'a demandée — et pire, le compteur de non-lus est périmé par construction dans cet état, donc afficher le point d'accent afficherait un nombre dont ce client ne peut pas répondre. Le DND est un **choix**, il explique le silence en entier, et il l'emporte sur le point pour la même raison.

**Le parseur de liens a trouvé un trou dans sa première version, par son propre test.** Un lien profond est une entrée que quelqu'un d'autre a écrite : l'OS la transmet depuis un clic. La première rédaction refusait les séparateurs par liste noire — et le test l'a cassée aussitôt, parce que `path_segments` rend le segment **encore percent-encodé** : `%2F..%2Fadmin` ne contient aucun `/` à cet endroit et en aurait produit un dans ce qui construit l'adresse suivante. C'est devenu une liste **blanche** de ce dont un id est fait, qui ne peut pas se tromper dans ce sens-là.

**Et la coquille est écrite, compilée par rien.** `web/src-tauri` est **hors du workspace cargo**, avec un `exclude` explicite : un membre casserait `cargo build --workspace`, `task test` et tous les gates pour quiconque travaille sur le serveur depuis une machine qui n'a aucune raison de bâtir une app de bureau. Le coût est dit plutôt que découvert — c'est exactement la forme de D17, écrit et jamais bâti, et ça le reste jusqu'à ce que l'image porte la pile, moment où le crate redevient membre et où les gates le ramassent. `task tauri:build`, `:dev` et `:check` passent par le doctor et rendent un paragraphe au lieu d'un mur d'erreurs de lien.

**Un bug attrapé en écrivant ces tâches, et il valait la peine** : `echo "… \`task dev:web\` …"` — les backticks dans une chaîne entre guillemets sont une substitution de commande, donc `task tauri:dev` *lançait* le serveur Vite qu'il se contentait de nommer.

**Puis la coquille a été finie jusqu'au point où le premier build sera un vrai test.** Elle ne l'était pas : il manquait `build.rs` — déclarer `tauri-build` en build-dependency et oublier ce fichier est la première façon dont un crate Tauri échoue, avec une erreur de macro qui ne nomme rien de tout ça —, le fichier `capabilities/`, que Tauri v2 exige pour chaque permission, et **les cinq icônes que le code nommait déjà**. Un premier build aurait rendu une file de fichiers absents plutôt qu'une réponse.

**Les icônes sont dérivées, pas commises telles quelles.** Sources SVG dans `icons/src/`, rasterisées par `task gen` (`xtask icons`), les deux commis. Un PNG seul arriverait en revue comme « binary files differ », qui est précisément le diff qu'une décision de design ne doit pas cacher. Et en Rust pur (`resvg`) : `librsvg`, Inkscape et ImageMagick sont des paquets système, donc une corvée qui en dépendrait serait une corvée que personne ne peut lancer dans un workspace Che — le mur exact qui bloque la coquille elle-même.

**Et les regarder a payé, une fois de plus.** La première rasterisation du `tray-dnd` rendait de la bouillie : la barre avait une largeur de coupe de 104 sur un canevas de 512, ce qui découpait la cloche en morceaux. Réduite, c'était mieux à 64 px et toujours illisible **à 32 px, qui est la taille réelle d'un tray**. La cause n'était pas la barre : les marques héritaient du `viewBox` de l'icône PWA, dessinée pour une plaque *maskable* et donc avec une marge large qu'un OS a le droit de rogner. La cloche remplissait 45 % de la sortie. `viewBox="96 108 320 320"` — recadré sur la marque — et les quatre états se lisent à 32 px. C'est la leçon de §10bis.3ter (« illisible en 16 px ») rencontrée par le seul chemin qui la démontre.

**Ce qui reste faux dans cette coquille est écrit dans son README** plutôt que découvert : le tray n'est pas *construit* (`main.rs` mappe un `Tray` sur un fichier et s'arrête), `navigate` pilote le front en `eval`-uant un `pushState` — ce qu'il y a de plus grossier qui marche —, rien n'y lit encore la session ni le cache, et il n'y a ni updater ni signature. Les cibles de bundle sont réduites à `deb` et `appimage`, parce que ce sont les seules dont les assets existent : `nsis` veut un `.ico`, `dmg` un `.icns`, et `tauri icon` les dérive du grand PNG le jour où quelqu'un bâtit sur ces plateformes.

**M4undetricies en détail.** La moitié que l'incrément précédent avait laissée écrite plutôt que faite.

**Pourquoi elle était laissée.** M4duodetricies a posé le SIGHUP et s'est arrêté avant les trust policies, délibérément : `[[trust_policy]]` était clonée *par valeur* dans `ExchangeToken` au démarrage, donc relire le fichier n'aurait rien changé au process. Adopter la valeur du fichier dans la configuration en mémoire aurait fait pire que rien — le réconciliateur journalise ces changements (§13, D28), donc le journal aurait annoncé un droit accordé pendant que la vérification continuait de le refuser. Une entrée d'audit fausse sur l'heure est pire qu'absente : une revue d'incident lui fait confiance. La dette a donc été écrite (D31) au lieu d'être contournée, et le comportement observé correspondait exactement à ce qui était documenté.

**Ce que ça demandait, et pas plus.** `LivePolicies` : un `Arc<RwLock<TrustPolicies>>` avec `adopt` et `current`. Pas d'`ArcSwap` — la lecture a lieu une fois par échange de jeton, c'est-à-dire une fois par job CI qui se connecte, pas sur un chemin chaud ; un `RwLock` n'y coûte rien de mesurable et ne coûte pas non plus une dépendance. Même raisonnement et même politique de poison que `ReconciliationLog`, qui vit dix lignes plus loin.

**Une seule copie, et c'est le vrai changement.** L'ensemble était construit **deux fois** : une pour l'échange, une pour la console, sur l'idée qu'un écran en lecture seule ne devrait pas être lié à un chemin d'authentification. Le SIGHUP retourne cet argument : deux copies, ce sont deux réponses à « quelle forge peut publier », et celle que la console affiche serait celle que personne n'applique. La console lit désormais le même objet — « pourquoi ce pipeline a-t-il été refusé » se répond avec la configuration en vigueur, pas avec celle d'hier.

**Le retrait est la moitié qui compte.** `adopt` remplace l'ensemble entier et ne fusionne jamais : une policy retirée du fichier doit *cesser* d'accorder, et c'est exactement ce qu'une fusion perdrait. C'est aussi la seule des deux opérations qui enlève un droit, donc la seule dont l'échec est un problème de sécurité plutôt qu'une gêne. Le verrou empoisonné est écrit dans le même sens : on écrit à travers, parce que refuser l'échange laisserait une policy **retirée** en vigueur pour la vie du process.

**Et une policy qui ne construit pas ne casse pas le reste du reload** : un motif malformé est une section fausse, et refuser les canaux à cause d'elle ferait qu'une coquille annule une édition sans rapport. L'ensemble courant reste, avec un avertissement.

**Vu, sur le process qui tourne** : un `[[trust_policy]]` ajouté au fichier puis `kill -HUP` — `trust_policy.added` journalisé, **plus aucun avertissement « non adopté »**, et la console affiche la policy sans redémarrage ; le fichier remis en état, `trust_policy.removed` journalisé. Le fichier, le process et le journal disent la même chose au même moment, ce qui est toute la dette.

**M4duodetricies en détail.** Une promesse répétée quatre fois dans ce document, et qu'aucune ligne de code ne tenait.

**Trouvée en écrivant D28, pas en la cherchant.** §11 dit « au boot et sur SIGHUP » ; §10.4bis veut « le dernier SIGHUP » sur la console ; la liste d'endpoints de §8 le répète ; et **T21**, un scénario de §3.4, exige « drift corrigé au SIGHUP ». Il n'y avait pas de gestionnaire de signal. Le réconciliateur tournait au démarrage et sur une minuterie, avec la **même** configuration en mémoire — donc éditer le fichier ne faisait rien jusqu'au prochain redémarrage.

**Et le test le disait déjà, à sa manière.** `critical_t21_a_managed_channel_refuses_api_mutation` porte le nom du scénario et n'en couvre qu'une clause sur trois. C'est la leçon de M4quatervicies dans une autre robe : le nom d'un test est une affirmation sur ce qu'il vérifie, et personne ne relit cette affirmation contre le texte qu'elle cite.

**Ce qu'un reload peut adopter, et pourquoi c'est si peu.** Deux sections : `[[channels.managed]]` et `[[service_accounts]]`. Ce sont celles que le réconciliateur applique *à la base*, donc les relire suffit. Tout le reste — la socket, le pool, le vérificateur OIDC, la flotte de sinks — a été câblé dans un objet vivant au démarrage, et relire la valeur ne changerait rien à ce qui la consulte. Un opérateur qui a édité `[database]` et envoyé un signal l'apprend par un avertissement nommé, exactement comme `unhonoured` le fait au démarrage : le silence, ici, c'est l'après-midi que quelqu'un passe à se demander pourquoi son édition n'a rien fait.

**Le piège, et c'est D28 qui le pose.** `[[trust_policy]]` décide quelle forge peut publier (§5.3), et le réconciliateur journalise désormais ses changements (§13). Si un reload lisait la version du fichier, le journal d'audit dirait « telle forge peut désormais publier » **pendant que le process continue de la refuser** — l'échange de jeton détient l'ensemble câblé au démarrage. Une entrée d'audit qui date un changement de droits de la mauvaise heure est pire qu'une entrée absente : une revue d'incident lui fait confiance. Donc un reload **garde l'ensemble courant**, le diff est vide, rien n'est journalisé, et l'entrée tombe au redémarrage qui l'adopte réellement. Le manque restant est écrit en **D31** plutôt que contourné en silence.

**`Trigger`, parce qu'un horodatage ne répond pas à la question.** §10.4bis demande « le dernier SIGHUP ». La passe tourne aussi toutes les soixante secondes, donc l'heure affichée est presque toujours celle de la minuterie, et un opérateur qui vient d'envoyer un signal ne peut pas savoir s'il a atterri. La console affiche maintenant `(démarrage)`, `(minuterie)` ou `(SIGHUP)`.

**Un reload n'est pas un redémarrage** : une configuration qui ne charge pas laisse la dernière bonne en place, avec un avertissement. Une coquille dans un fichier ne doit pas coucher un replica qui sert — surtout pas à la seconde où quelqu'un est en train de l'éditer.

**Vu, sur le process qui tourne** : `kill -HUP` → « configuration re-read on SIGHUP » puis une passe `trigger: Sighup` ; un `[[channels.managed]]` ajouté au fichier crée le canal et écrit `managed.channel.added` avec l'acteur `configuration` ; un `[[trust_policy]]` ajouté produit l'avertissement et **`journalled: 0`** ; le fichier remis en état archive le canal ; et la passe de minuterie suivante retombe à zéro.

**M4septvicies en détail.** Une dette qui demandait trois décisions, et dont la mécanique n'était petite qu'une fois les trois prises.

**Ce que D28 avait raison de refuser.** Elle disait « la mécanique est petite, la décision ne l'est pas » et listait trois questions. Elles n'étaient pas rhétoriques : chacune, tranchée à l'envers, produisait un journal pire que pas de journal.

**L'acteur.** §5 compte quatre principaux et les fait tous *authentifiés* — une personne, un bot, une clé de canal, un job CI. Le réconciliateur n'est aucun des quatre : il applique un fichier, au démarrage ou sur `SIGHUP`, et personne n'a « fait » ça. C'est exactement pourquoi la promesse de §13 est restée sans émetteur si longtemps — **il n'y avait pas de valeur honnête à mettre dans ce champ**, et M4duovicies avait retiré `trust_policy.changed` du vocabulaire plutôt que de le garder sans producteur. L'élargissement se fait **dans le port d'audit seul**. Un cinquième `Principal` aurait été représentable partout où un principal l'est — une adhésion, une publication, un receipt — et chaque `match` sur les quatre aurait gagné un bras signifiant « ne peut pas arriver ». La configuration est un acteur du *journal* et de rien d'autre, donc elle est nommée là et nulle part ailleurs. La forme sur le fil reste plate : `{"kind":"configuration"}` à côté de `{"kind":"user","id":…}`, donc un lecteur qui aiguillait déjà sur `kind` gagne un cas et pas un niveau d'imbrication — le front n'a pas changé d'une ligne.

**La granularité, et ce qu'elle a coûté.** Une entrée par passe aurait été triviale et inutile : elle dit que quelque chose a été appliqué, ce qui laisse l'opérateur devant `git log`, c'est-à-dire devant le trou que §13 promet de combler. Une entrée par changement demande de savoir ce qui a changé, et le réconciliateur ne le sait pas — il applique le fichier **entier** à chaque passe. Pour les canaux et les comptes on aurait pu diffuser contre les lignes vivantes ; pour les trust policies il n'y avait **rien du tout**, puisqu'elles sont lues en mémoire au démarrage et stockées nulle part. D'où `managed_declarations`.

**Et ce qu'elle stocke est la déclaration, pas l'entité.** La ligne vivante d'un canal managé dérive pour des raisons qui ne sont pas le fichier : un manager édite ce qu'il a le droit d'éditer. Diffuser contre elle rapporterait l'action d'un humain comme un changement GitOps — et la rapporterait **à chaque passe** jusqu'à ce que quelqu'un revienne en arrière. Ce que §13 demande est de savoir si le *fichier* a bougé.

**Une instance stable n'écrit rien**, et c'est une condition et non un effet : la passe tourne toutes les soixante secondes, donc une ligne par passe enterrerait sous elle exactement les lignes que §13 dit de garder — le paragraphe « ce qui n'y va pas » de cette même section parle de ça. Un test le tient, et il est le seul qui échoue si l'on oublie de réécrire l'instantané.

**La portée : les trois.** D28 prévenait qu'en traiter une seule « réglerait un tiers d'un problème en donnant l'impression du tout ». Le même réconciliateur écrit les trust policies, les canaux managés et les comptes de service, et aucun des trois n'était journalisé. Les trois le sont — avec une asymétrie assumée : il n'existe pas de `managed.service_account.removed`, parce que rien ne retire un compte de service. Un compte qui quitte le fichier reste, ses clés et son historique avec lui, et inventer une action qu'aucun chemin de code ne produit est précisément ce que M4duovicies avait nettoyé.

**Deux détails qui se lisent mal si on ne les écrit pas.** Une trust policy est clé par *issuer et subject*, pas par issuer : §5.3 laisse une forge porter plusieurs motifs de subject aux droits différents, et une clé sur l'issuer seul rendrait « un second motif de branche a été accordé » indistinguable de « celui qui existait a été édité ». Et une entrée de canal managé porte son `channel`, pas les deux autres : une trust policy peut en accorder plusieurs, donc en nommer un serait une demi-vérité.

**M4sexvicies en détail.** La dernière des trois « écrit, jamais vu », fermée par le composant dont on avait écrit qu'il ne servirait pas.

**Ce qui avait été mal lu, encore une fois par nous.** M4quatervicies s'était terminé sur une phrase nette : « D19 reste ouverte, et c'est la seule des trois que le sidecar n'aide pas : `--notify-send` vise un démon de bureau, pas une page. » La conclusion était juste, la prémisse trop étroite. Le sidecar n'est pas *un navigateur*, c'est **une image qui contient un navigateur** — et Chrome dépend de D-Bus, donc `dbus-daemon` y est. Il n'est nulle part ailleurs dans ce pod : `tools` ne l'a pas, `mise` ne le connaît pas, et sans root il ne s'installe pas. Ce qu'on cherchait était à côté depuis M4tervicies.

**Deux conteneurs, une socket.** `browser` et `tools` montent tous les deux les sources (`mountSources: true` — décidé en M4tervicies pour les téléchargements et les traces, pas pour ça). Une socket de bus de session posée sur ce chemin est donc ouverte d'un côté et jointe de l'autre. `task desktop:bus` lance le `dbus-daemon` dans le sidecar, par `kubectl exec`, parce que c'est le seul endroit où le binaire existe.

**Et le démon, on l'écrit.** `org.freedesktop.Notifications`, ce sont quatre méthodes d'une spécification publiée. `xtask notifyd` les répond et imprime chaque `Notify` en une ligne de JSON. Même arbitrage que `dev/browser/cdp.mjs` contre Playwright — le protocole est petit, la dépendance ne l'est pas — avec un argument que CDP n'avait pas : **un stub qu'on possède peut affirmer ce qui est arrivé**. Un vrai `dunst` dessinerait une bulle que personne, dans ce pod, ne peut regarder ; celui-ci dit `{"app_name":"weebo-notify","summary":"etcd degraded",…}`. Il lit les huit arguments de `susssasa{sv}i` strictement, donc un message malformé échoue là plutôt que d'être dessiné vide. `dbus` entre au manifeste de `xtask` sans entrer au build : la feature `d` de `notify-rust` est déjà ces bindings-là.

**Le vrai défaut était dans le code testé, pas dans l'environnement.** `relay()` avalait ses erreurs — délibérément, et la raison tient toujours : une machine sans démon de notification est le cas normal pour ce CLI, et un TUI qui signalerait une bulle ratée à chaque arrivée serait pire que muet. Mais une fonction qui avale ses propres erreurs **ne peut être prouvée par rien**. D19 disait « écrite, jamais vue » ; il manquait un démon *et* toute façon de distinguer une livraison d'un non-événement. La fonction est coupée en deux : `show()` rend son résultat, `relay()` l'ignore un niveau plus haut. Le silence est conservé exactement là où il était voulu, et devient observable partout ailleurs.

**Le test saute plutôt que d'échouer** sans `DBUS_SESSION_BUS_ADDRESS`, comme la conformité Postgres saute sans `WNK_TEST_DATABASE_URL` — et le renversement mérite d'être noté : hors Che, un poste avec une session de bureau le fait tourner contre GNOME ou KDE, où il veut dire davantage qu'ici. Falsifié dans les deux sens : sans démon sur le bus il échoue, sans bus il saute.

**Ce que ça ne prouve pas.** Qu'une bulle est jolie, qu'elle apparaît au bon endroit, qu'elle survit à un thème. Rien de ce qui tourne dans ce pod ne peut le dire, et §9.2 garde son autre aveu — « quand le terminal n'a pas le focus » reste non implémentable de façon portable, et le drapeau reste le commutateur.

**M4quinvicies en détail.** Une dette qui demandait une décision avant du code, et qui avait raison de le demander.

**Ce que D27 refusait de laisser faire.** Elle était ouverte depuis M4duovicies avec une consigne inhabituelle : *ne pas enchaîner*. « Non lu » n'est pas défini par §7.3, qui décrit des receipts `seen` **opt-in par canal** ; sur un canal qui ne collecte rien, personne ne pose de `seen`. Choisir en écrivant le code produirait « un badge dont personne ne saurait dire ce qu'il compte ». C'est exactement ce qui a failli se passer : le port, l'adaptateur Postgres et le cas d'usage étaient écrits — et le commentaire de `CountUnread` affirmait déjà que « `seen` est enregistré sur tous les canaux depuis l'incrément qui a ajouté ceci », **ce qui n'était vrai nulle part**. `PostReceipt` sortait toujours en `Ok(None)` avant d'écrire quoi que ce soit. La documentation avait pris la décision ; le code ne l'avait pas suivie.

**Ce que l'opt-in gouverne.** §7.3 le dit en toutes lettres si on le relit : « **si activé sur le canal** : les membres autorisés **voient** « vu par 3/5 » ». L'opt-in porte sur ce qui est *montré*, pas sur ce qui est *écrit*. Et le défaut compte, parce que `receipts_enabled` vaut **`false`** : ce n'est pas un cas exotique, c'est l'état de tout canal qu'un opérateur n'a pas configuré. Un badge qui compte les lignes sans `seen` y serait resté **égal à la taille du canal**, pour toujours.

**Et le correctif ne s'achète pas avec une fuite.** C'est le point à relire. Écrire le `seen` partout ne suffit pas : `ReceiptRepository::record` appendait l'événement en `Audience::Channel`, donc enregistrer une lecture sur un canal qui ne collecte rien l'aurait **diffusée à tous ses membres** — précisément ce dont le canal s'était désabonné. D'où `Visibility` sur le port : la ligne est écrite dans les deux cas, seul l'événement change d'adresse. `Channel` pour « vu par 3/5 », `Reader` pour un `Audience::User` qui ne sort pas des connexions du lecteur — ce qui n'est pas rien non plus, puisque lire sur son téléphone doit faire tomber le badge sur son portable. Un principal sans inbox (service account, clé de canal, job CI) n'a pas d'audience privée : la ligne est écrite, rien n'est appendu. `delivered` garde l'ancienne porte — il est par device, ne nourrit que le panneau de lecture, aucun badge n'en dépend, et seul `seen` avait gagné l'exception.

**Le port n'a pas de politique, et c'est pourquoi `Visibility` est un paramètre.** Le dépôt aurait pu lire `receipts_enabled` lui-même. Il ne le fait pas : une décision de « qui a le droit de voir » qui descend dans un dépôt est un contrôle d'autorisation qui a changé de couche.

**Ce que la moitié serveur ne suffisait pas à faire.** Le premier essai dans le navigateur a montré un badge qui ne bougeait pas d'un canal sans receipts, **après la correction serveur**. Le front portait la même mauvaise lecture, écrite noir sur blanc : `NotificationDetail.vue` ne postait le `seen` que `if (id && collects)`, avec en commentaire « seulement là où le canal collecte, pour ne pas dire à un canal qui ne collecte pas qui a lu quoi ». L'intention était juste et l'endroit était faux — c'est le serveur qui tient cette garantie maintenant, et la garder ici en plus revenait à ne jamais marquer lu. Deux couches, une même phrase de §7.3, mal lue deux fois.

**`GET /unread`, et pas `GET /channels`.** Le badge change à chaque publication et à chaque lecture ; une liste de canaux change quand un opérateur édite une configuration. Les replier l'un dans l'autre, c'est soit refetcher la liste sans cesse, soit afficher un badge aussi frais qu'une configuration. La route est plate — à côté de `/search`, `/sync`, `/receipts` — parce que c'est une question sur tout ce que l'appelant peut lire, pas sur une ressource ; un préfixe `/inbox` n'aurait eu qu'un seul membre. Un canal sans non-lu est **absent** de la réponse plutôt que présent à zéro, pour que « pas de badge » ait une seule écriture ; `countOf` et le TUI retransforment l'absence en zéro sans jamais la stocker.

**Le compte est celui du serveur**, jamais celui des lignes que l'onglet détient. Un badge replié sur la page chargée voudrait dire « non lus parmi les cinquante lignes que j'ai », un nombre différent qui ressemble exactement au bon. Le flux ne transporte donc pas le compte : un événement dit seulement *redemande*, et `refresh` coalesce la rafale — un déploiement qui fane out quarante révisions est quarante événements pour un nombre affiché une fois par frame.

**Vu, et deux défauts corrigés en regardant.** La méthode de M4quatervicies appliquée à son successeur. Le badge du canal `Sink demo` est passé de 11 à 10 en ouvrant une notification, sans rechargement, **sur un canal qui ne collecte pas de receipts** — c'est-à-dire précisément le chemin qui était cassé. Et le lecteur d'écran s'entendait dire « **1 non lues** » : le français accorde le participe avec le compte, le badge n'affiche qu'un chiffre, et le seul destinataire de cette phrase est celui qui ne voit pas le chiffre. `unreadLabel` porte la correction et son test.

**Ce que cet incrément ne prend pas.** Le tray et le badge dock de §10bis.3bis/3ter. `Session::unread()` est dans `notify-client-core` pour que « trois rendus, une seule vérité » soit vrai quand ils existeront ; il n'existe aucune coquille Tauri, M5 étant bloqué (D13). Deux rendus sur trois, et le troisième attend son shell plutôt que son état.

**M4quatervicies en détail.** Le premier incrément de ce projet dont la méthode est « ouvrir la page ».

**Ce qui manquait n'était pas le navigateur.** M4tervicies en a planifié un — Chrome sous Xvfb, CDP en interne, noVNC sur une route publique — et s'est arrêté là, ce qui laissait D19, D21 et D26 exactement où elles étaient : un navigateur que personne ne pilote est un navigateur que personne ne regarde. La moitié manquante est `dev/browser/cdp.mjs` et le namespace `browser:` du Taskfile.

**Sans une dépendance**, et ce n'est pas de l'ascèse. Node 24 livre un `WebSocket` global, CDP est du JSON sur une socket : le client entier fait soixante lignes, moins que l'entrée de lockfile qu'une alternative coûterait. Playwright ou puppeteer téléchargeraient un navigateur que ce pod n'a aucune raison d'aller chercher — il en a déjà un, planifié par le devfile — puis on leur demanderait de s'y connecter *en CDP* de toute façon. C'est la règle que ce dépôt s'était donnée pour la clé VAPID et le bcrypt de Dex, appliquée une fois de plus : rien ici ne suppose un outil que l'image pourrait ne pas livrer, et Node n'est pas une supposition puisque le front ne se construit pas sans lui.

**Il pilote la page qu'un humain regarde**, délibérément : la cible existante et non une nouvelle depuis `/json/new`. Tout l'intérêt du sidecar est la route noVNC — quelqu'un regarde pendant que le script travaille. Un pilote qui ouvrirait son propre onglet headless déplacerait hors champ précisément ce qu'on essaie de voir.

**Puis on a regardé, et voici ce que ça a rendu.**

**Le défaut que personne ne cherchait (§2.4).** Chaque ligne d'inbox, chaque entrée de timeline et chaque toast affichaient `usr_01KZ3RCPRRTH2J18W5WSN2AVGT`. §2.4 finit sur une phrase qui se lit comme un résumé et qui est une exigence — « le profil s'affiche **partout** où un principal apparaît : `author` des révisions et interactions, "vu par 3/5", liste des membres » — et sur ces trois-là, **une seule** était câblée. `Author::named` avait un unique appelant de production dans tout le workspace, le rôle des receipts ; `publish`, `revise` et `interact` construisaient tous un `Author::anonymous`.

Pourquoi c'est resté : rien n'était *cassé*. Les clients ont un repli documenté vers la référence pour les principaux sans profil — une clé de canal anonyme — et un user nommé recevait exactement ce repli. Un test qui affirme « la ligne affiche le label de l'auteur » **passe sur un ULID**. C'est la même leçon que M4duovicies sous une autre forme : le test et le code étaient d'accord, et tous les deux avaient tort sur ce que la page devait montrer.

`crates/notify-app/src/attribution.rs` porte la correction et son raisonnement. Le point à relire est **où** nommer. Deux endroits étaient possibles et un seul est juste pour les *deux* lecteurs d'une même révision : une révision atteint un client deux fois, comme événement SSE émis par le publish (§8.1) puis comme ligne relue plus tard. Nommer à la lecture ne nomme que la seconde — le toast dirait `usr_01K…` et la même notification après rechargement dirait « Alice Martin ». Nommer à l'écriture rend les deux identiques par construction, puisque c'est la même valeur. Le coût est écrit plutôt que découvert : **un renommage ne remonte pas le temps**. Pour une timeline — un journal de ce qui s'est passé — c'est défendable ; pour le rôle des receipts, qui répond « qui l'a lu *maintenant* », ça ne l'est pas, et c'est pourquoi ce rôle continue d'hydrater à la lecture et que ce module n'y touche pas. Deux questions, deux réponses.

Vérifié contre le vrai Postgres : la réponse du publish porte `"display_name": "Alice Martin"`, la ligne l'affiche, et **les lignes publiées avant la correction gardent leur ULID juste au-dessus** — la preuve des deux d'un coup, sur une seule capture. Ces lignes-là ne seront pas rattrapées et n'ont pas à l'être : aucun déploiement réel n'a jamais tourné, donc les seules concernées sont dans une base de développement (**D29**).

**D26 — l'image jointe (soldée).** Le panneau détail avait un `<img>` gardé par `image.kind === 'data_uri'`. Or la CLI bascule en attachment au-dessus de 48 KiB (§9.1) : un publisher qui joignait une capture la regardait disparaître, sans erreur nulle part. `lib/attachment.ts` résout les deux genres — un `data:` est déjà les octets, un attachment demande sa signature à `GET /attachments/{id}` (§6.1, la route de M4unvicies, qui n'avait toujours aucun appelant).

Une URL signée et non un blob, contrairement à `lib/avatar.ts` d'à côté, et la différence n'est pas stylistique : un avatar est servi *par le serveur* et réclame l'en-tête `Authorization` qu'un `<img>` ne peut pas envoyer, tandis que la signature d'un attachment voyage **dans l'URL** — donc aucun en-tête, et tirer 25 MiB à travers le tas de cet onglet annulerait la raison d'être du flux client ↔ bucket. Le cache expire, lui, contrairement à celui des avatars : un id d'avatar est immuable, une signature vit quelques minutes, et une inbox laissée ouverte re-rendrait une URL que le bucket a commencé à refuser — ce qui ressemble exactement à une image cassée.

**Une surface sur quatre, et c'est le serveur qui décide de la suite.** §10.3 fait de la notification « un composant unique décliné » en row d'inbox, card détail, toast et notification OS ; l'image n'est câblée que dans la card. La raison n'est pas le temps : un attachment **n'a pas de vignette**. §6.1 en fait un flux client ↔ bucket sans traitement serveur, exactement là où §2.4 fait redimensionner l'avatar en 256×256 à l'ingestion — donc une vignette de row tirerait l'objet entier, jusqu'à 25 MiB, une fois par ligne visible dans une liste virtualisée. La faire d'abord côté vue serait un incident de bande passante déguisé en fonctionnalité. Écrit en **D30** avec sa réparation : une dérivée à l'ingestion, comme pour les avatars, et une route qui la sert.

**Et la CSP, qui est le vrai piège.** Une URL présignée ne pointe **pas** sur l'origine de l'API, et `img-src` ne nommait que celle-ci : un bundle construit bloque l'image que le serveur vient de signer. Pire, c'est invisible en développement — `vite.config.ts` ne pose la politique qu'au build, donc l'attachment s'affiche parfaitement sous `pnpm dev` et nulle part ailleurs. `VITE_ATTACHMENT_ORIGIN` nomme l'origine du stockage ; il est **optionnel**, parce qu'un déploiement sans objets est un déploiement qui marche, et son absence donne la légende « L'image jointe n'a pas pu être affichée » plutôt qu'un trou. Il ne peut pas être déduit — l'URL vient du `[storage] endpoint` du serveur, que ce bundle n'a jamais vu — ni lu à l'exécution, une politique étant figée au parsing du document. Deux tests le tiennent, et ce sont les seuls instruments capables de voir cet écart avant un déploiement.

**D21 — la palette (soldée).** Elle marchait : `Cmd+K` ouvre, `GET /search` répond, les groupes de §10.4 sont là. Trois choses qu'aucune suite jsdom ne peut demander :

- **le curseur clavier et le survol souris rendaient le même gris** (`bg-accent` contre `hover:bg-accent/50`). Deux lignes paraissaient également choisies et `Entrée` devenait un pari. La ligne active porte maintenant une barre à gauche et une graisse en plus du fond — la règle « jamais la couleur seule » que §10.3 applique aux statuts, pour la même raison ;
- **passer de la première ligne à la dernière sélectionnait hors écran.** La liste défile (`max-h-[50vh]`) et la sélection est la nôtre, donc rien ne défilait tout seul. Une palette qu'on ne peut pas piloter jusqu'à sa propre dernière ligne n'est pas la surface sans souris que §10.4 demande ;
- **un lecteur d'écran n'apprenait rien** : un `<div>` de `<button>`s n'a pas de sélection à annoncer. C'est une listbox maintenant, avec `aria-activedescendant`.

**D7 — Dex (soldée, et sa jambe était rouge).** Il restait « à le démarrer une fois ». Démarré, la matrice `oidc-compat` a échoué sur son premier passage : le test affirmait `claims.subject == username` pour tous les fournisseurs, et Dex répond un `sub` opaque — un base64 de son id interne et du connecteur. **Dex a raison et l'assertion avait tort** : OIDC fait de `sub` une valeur opaque choisie par le fournisseur, rien n'autorise un relying party à y lire un nom d'utilisateur, et l'égalité ne tenait que là où cette suite l'avait *arrangée*. La propriété dont le code dépend vraiment (§5.1 : un user est retrouvé par `oidc_subject`) est la **stabilité**, qui n'était affirmée nulle part et l'est maintenant pour tout le monde — un sujet qui changerait entre deux connexions créerait un second compte ne portant aucun des canaux du premier, avec un symptôme apparaissant des jours plus tard, loin de sa cause. C'est exactement ce que §15 attend de cette matrice : une lecture indépendante d'OIDC, et non trois exécutions du même fournisseur.

**Et l'icône de la PWA ne s'est jamais décodée** (M3), trouvée dans la console plutôt que cherchée. Le SVG est servi en `200` avec le bon type MIME et Chrome répond `Download error or resource isn't a valid image` : son commentaire nommait les tokens CSS `--background` et `--foreground` en toutes lettres, et **XML interdit un double tiret à l'intérieur d'un commentaire**. Le document ne parsait pas, donc l'application installée n'avait pas d'icône — et rien n'échouait ailleurs, parce que `public/` est recopié tel quel dans le bundle : rien là-dedans n'est compilé, typé, linté ni importé par un test, et un manifeste n'est lu que par un système d'exploitation, une fois, au moment de l'installation. C'est le moment le moins observable de ce projet. `web/src/lib/pwa.spec.ts` tient maintenant les deux propriétés que ces fichiers perdent en silence, et il a été falsifié en remettant les tirets.

**D19 reste ouverte**, et c'est la seule des trois que le sidecar n'aide pas : `--notify-send` vise un démon de bureau, pas une page. Le navigateur ne répond pas à cette question-là.

**M4tervicies en détail.** Une contrainte relue plutôt qu'un manque comblé.

**Ce qui a été mal lu, et par qui.** D7, D13 et D17 partageaient une phrase — « aucun moteur de conteneurs ici » — et cette phrase est vraie. Ce qu'elle ne dit pas, et ce que §12.4 décrit pourtant en entier, c'est qu'un devfile **planifie des conteneurs** : Postgres, RustFS et Valkey tournent ici depuis M1 sans qu'aucun `docker` n'existe. Trois dettes attendaient donc un environnement qu'elles avaient déjà pour deux d'entre elles.

**Dex entre (D7).** Un sidecar, `sleep infinity` plutôt que `dex serve` : sa configuration n'existe pas tant que `task dev:dex` ne l'a pas rendue (le mot de passe est généré par run, §3.2), et un conteneur dont la commande réclame un fichier que le workspace n'a pas écrit boucle en crash à chaque démarrage. Il attend ; la commande `run-dex` lance le process quand le fichier est là.

**Et `render.sh` ne pouvait pas tourner ici du tout** — trouvé en l'exécutant, pas en le lisant. Il veut bcrypt, il connaît `caddy` et `htpasswd`, et un workspace Che n'a ni l'un ni l'autre : mise livre des outils, pas Apache. La troisième branche est donc `cargo run -p xtask -- bcrypt-hash`, ce qui applique à la lettre la règle que ce dépôt s'était déjà donnée pour la clé VAPID — « Rust plutôt qu'openssl : rien ici ne suppose un outil que l'image Che pourrait ne pas livrer ». Le mot de passe arrive sur stdin et non en argument, contrairement à la branche caddy d'à côté : `ps` montre un argument à tout le monde. Sortie `$2a$`, la forme exacte des deux autres branches, parce qu'un quatrième préfixe serait une quatrième chose à vérifier pour rien.

**La pile Tauri n'entre pas, et c'est la moitié où la relecture avait tort (D13).** Un sidecar `rust:1-bookworm` plus une commande d'installation a été écrit, puis retiré : un DevWorkspace applique sa security context au **pod entier**, donc aucun conteneur n'y est root et `apt-get` y échoue exactement comme dans `tools`. « Planifier un conteneur » et « être root dedans » sont deux capacités différentes, et D13 demande la seconde — la phrase d'origine de §12.4 avait raison, et la correction que cet incrément lui apportait était fausse. Ce qui reste est le vrai correctif : le diff amont dans `docs/upstream/che-min-mise-tauri.md`, plus la raison pour laquelle il n'a pas de contournement local, écrite là pour que personne ne repasse l'après-midi dessus.

**Un navigateur entre à la place (D19, D21, D26).** `che-browser`, l'image voisine dans le même dépôt amont : Chrome sous Xvfb, CDP et WebDriver en interne, **noVNC sur une route publique**. Ces trois dettes sont ouvertes avec le même mot — « écrit, jamais vu » — et c'est littéralement ce qui manquait : la suite web tourne sous jsdom, qui répond sur la logique et pas sur l'allure d'une palette ni sur une image qui décale sa ligne en chargeant. Le navigateur vit dans ce composant, ce qui le pilote vit dans `tools`, et tout se joint sur `localhost`. Il monte les sources, contrairement au devfile de référence amont : « il rend des pages, il n'a pas à lire le dépôt » vaut pour un screenshot CDP, dont les octets reviennent sur le fil, et cesse de valoir dès qu'il y a un fichier à se passer — téléchargement, trace, artefact relu par le pilote, `dist/` ouvert en `file://`.

**Les deux Containerfiles existent (D17).** Le serveur en distroless `cc-debian12:nonroot` — la pile TLS est rustls de bout en bout, donc aucun libssl à porter et aucune CVE OpenSSL à hériter ; le bundle web derrière `nginx-unprivileged`, qui émet enfin les trois en-têtes qu'un `<meta>` ne peut pas porter : `frame-ancestors 'none'`, `report-to` et `Strict-Transport-Security`. La politique du serveur web est une **seconde** politique et non un remplacement — un navigateur applique les deux —, ce qui évite la copie que D22 a passé deux incréments à supprimer.

Trois décisions valent d'être relues. Le **`report-to` pointe par défaut sur un `return 204`** de ce même nginx : un `report-to` qui nomme un endpoint inexistant est une politique qui journalise ses échecs nulle part, c'est-à-dire l'état qu'on voulait quitter. Les **en-têtes vivent dans une seule `location`**, parce qu'`add_header` dans un bloc *jette* tout ce qui est hérité du serveur — des règles de cache par chemin auraient silencieusement supprimé les en-têtes de sécurité sur exactement les fichiers qui ont une règle. Et le **stub des dépendances est refusé** : avec vingt et un crates, c'est vingt et un chemins à tenir à jour, et le jour où quelqu'un ajoute le vingt-deuxième rien n'échoue — le cache cesse juste d'aider.

**Vérifié ici, et c'est plus que prévu** : `trivy config` lit les deux fichiers sans moteur ni build, et il a **fait échouer le gate** — DS-0002 (HIGH, pas de `USER`) sur l'image web, alors que `task audit` refuse à partir de HIGH. L'image de base tourne déjà en 101, mais trivy lit le fichier et pas la base dont il hérite ; la ligne est donc écrite, et elle sert deux fois puisqu'une base qui repasserait root un jour ne peut plus promouvoir celle-ci en silence. Les deux fichiers sont propres au gate maintenant. Ce qui reste non vérifié est nommé : **aucune image n'a jamais été construite**, la première exécution est sur le runner Forgejo, exactement comme D7 et D12.

**Deux bugs préexistants, trouvés en rejouant les portes** plutôt qu'en écrivant du neuf, et tous deux sur le commit précédent :

- **`task lint:check` était rouge**, sur un tri d'imports que `task lint` corrige — donc le hook pre-commit ne l'a pas appliqué et le job `check` aurait été rouge au push ;
- **l'avatar ne s'affichait jamais.** `showAvatar` n'avait aucun appelant : `avatarUrl` restait `null`, le `v-if` cachait l'`<img>`, et les initiales de repli rendaient exactement ce qu'elles rendent pour quelqu'un sans avatar — un échec parfaitement silencieux, et le troisième « méthode sans appelant » de la série après D26 et D27. L'appel va dans `hydrate`, l'entonnoir unique par lequel `load`, `save`, `release` et `upload` passent tous ; le câbler à un seul appelant est précisément comment on obtient une fonction qui n'en a aucun.

**M4duovicies en détail.** Le premier vrai bug de la série, et le seul qui cassait quelque chose en production.

**Le balayage a continué.** M4unvicies avait cherché les méthodes de port sans appelant ; celui-ci a pris la classe suivante — **les vocabulaires fermés** : les actions d'audit, les familles de métriques, les codes d'erreur. La première a suffi.

**Ce qui a été trouvé.** `AuditEntry.action` était un `&'static str`, les émetteurs écrivaient des littéraux, et la liste fermée contre laquelle un adapter les vérifie vivait ailleurs, **privée, dans cet adapter**. Les deux avaient dérivé **dans les deux sens** :

- `trust_policy.changed` était dans la liste et personne ne l'émettait ;
- `channel.policy_updated` et `channel.member_left` étaient **émis** et absents de la liste.

**Et la vérification a lieu à la lecture.** `record` accepte n'importe quelle chaîne ; `intern_action` ne tourne que quand une ligne remonte. Donc un seul `PATCH /channels/{id}` — ou n'importe qui quittant un canal — écrivait une ligne qui faisait **échouer `GET /admin/audit` pour la page entière**, pas pour la ligne. Le journal dont §13 dit « c'est celui qu'on rejoue après un incident » se cassait sur une édition ordinaire, et l'onglet audit de la console plateforme (§10.4bis) avec lui.

**Prouvé avant d'être corrigé**, contre le vrai Postgres, et le message est exactement celui qu'un opérateur aurait vu : `stored audit.action could not be read back: 'channel.policy_updated' is not in the audit vocabulary of this build`.

**Pourquoi personne ne l'avait vu**, et c'est la partie qui vaut d'être retenue : il *y avait* un test, `the_vocabulary_covers_what_the_use_cases_actually_record`, dont le commentaire disait « a mismatch would store an entry that cannot be read back, which is the one failure mode an audit log must not have ». Il tenait sa **propre troisième copie** de la liste, à jour au moment où il a été écrit, et il passait. Un miroir écrit à la main d'une liste écrite à la main est d'accord avec lui-même et avec rien d'autre. C'est la leçon de D22 sous une forme que D22 n'avait pas prévue : la duplication ne se trouvait pas dans du code exécuté, mais entre du code et son test.

**La correction ne se contente pas des deux cas.** Le champ n'est plus une chaîne : `AuditAction` est un newtype dont le contenu est privé, dont les seules valeurs sont les constantes de `notify_ports::actions`, et dont `parse` — la seule décision d'acceptation, désormais unique — lit exactement la même liste. Un émetteur ne peut plus inventer une valeur, et une valeur qui existe est lisible par construction. C'est le même raisonnement que le `Quota` fermé de §8.2 (« ajouter une variante ne compile plus tant que personne n'a décidé ce qu'elle vaut »), appliqué là où il manquait.

**Ce que ça coûte, dit honnêtement** : les comparaisons de test passent par `.as_str()`, et `AuditQuery.action` reste une `String` — un filtre peut nommer ce qu'il veut, un nom inconnu ne matche simplement rien, et refuser l'entrée d'un filtre serait un `400` là où « zéro résultat » est la bonne réponse.

**`trust_policy.changed` est parti** plutôt que d'être émis à la va-vite. §13 promet que les changements de trust policy sont journalisés ; elles viennent de GitOps et le réconciliateur n'audite rien du tout aujourd'hui. Émettre une action au réconcile demande une détection de changement et une décision sur l'acteur (personne n'a « fait » l'action — c'est un fichier). Écrit en **D28** plutôt que deviné, et la liste ne porte plus de valeur que rien ne produit — ce qui était la moitié inoffensive du même défaut.

**M4unvicies en détail.** Une pièce jointe qu'on pouvait envoyer et que personne ne pouvait lire.

**Comment elle a été trouvée.** Plus par lecture de section mais par **balayage mécanique**, la méthode que les trois incréments précédents ont rendue évidente : les trous vivent là où quelque chose est déclaré et rien ne le lit. Trois classes passées en revue — les constantes de `limits.rs` (toutes lues, depuis M4duodecies), les clés de configuration (soldé par M4octodecies), et **les méthodes de port sans appelant**. La dernière rend deux résultats : `AttachmentStore::presign_download` et `ReceiptRepository::has_seen`.

**`presign_download` : le port existait, l'adapter S3 l'implémentait, et rien ne l'appelait.** Aucune route non plus — §8 ne listait que `POST /attachments/presign`. Or le bucket est **strictement privé** et « presign = seul chemin d'accès » (§6.1), donc il n'existait littéralement aucune URL par laquelle un client pouvait obtenir les octets. Une notification portant `image: {kind: "attachment", ref: "att_…"}` était **impossible à afficher**, par n'importe quel client. La CLI sait produire ces références depuis M4septies (au-dessus de 48 KiB, `--image` bascule en attachment) : le chemin d'écriture est complet depuis des mois et le chemin de lecture n'a jamais existé. C'est exactement D14 — « §8 le liste, aucune route ne le sert » — dans l'autre sens, et personne ne l'a vu parce que le côté écriture, lui, marchait.

**Une URL signée et non les octets**, contrairement à la route avatar d'à côté. Ce n'est pas une préférence : §6.1 fait des attachments un flux **client ↔ bucket** dans les deux sens, ce qui garde une capture de 25 MiB hors du tas de ce process. L'avatar est l'exception documentée, et il l'est parce que le serveur doit de toute façon le décoder et le redimensionner (§2.4).

**L'autorisation était la vraie question**, puisqu'un attachment n'a **aucun contrôle d'accès propre** : l'objet n'est pas lié à un canal (§6.1, et la fin de D14 le disait déjà), et son identifiant est un ULID nu. Ce qui donne droit aux octets est donc **de pouvoir lire quelque chose qui les affiche** — une question sur les *notifications*, pas sur l'objet. `attachment_is_visible(id, canaux_lisibles)` la pose, en réutilisant `Authorizer::readable_channels` qui répondait déjà pour l'inbox et la recherche.

**Le refus est `notification.gone` et pas une erreur de scope**, et c'est délibéré : « vous n'avez pas le droit » et « cet objet n'existe pas » doivent être indiscernables de l'extérieur, sinon l'endpoint répond, une requête à la fois, quels identifiants existent sur l'instance. Même raisonnement que la révocation du device de quelqu'un d'autre.

**Deux propriétés que les tests tiennent et qui ne sont pas évidentes** : un identifiant presigné mais **jamais publié** n'est pas téléchargeable — ce qui autorise une lecture est une notification, donc un upload que son auteur n'a jamais publié reste privé ; et une référence dont **l'upload n'est jamais arrivé** n'est pas signée, parce qu'un presign est une promesse et pas un upload, et qu'une URL signée vers des octets absents donne au client une image cassée qu'il ne peut pas distinguer d'un bucket cassé.

**La visibilité s'éteint exactement quand la collecte s'allume.** `attachment_is_visible` exclut les lignes supprimées et expirées, c'est-à-dire au même instant où le balayage de M4novodecies commence à considérer l'objet comme collectable. Les deux réponses coïncident, ce qui est ce qui empêche un lecteur de tenir une URL vers des octets sur le point de disparaître.

**Falsifié** : la clause `AND n.channel_id = ANY($2)` retirée, le test échoue contre la vraie base sur « a channel that does not show it grants nothing ». C'est la clause qui porte toute la sécurité de cette route.

**Ce qui reste, et est ouvert en D26** : le web ne rend **toujours pas** les images de notification. Un `rtk grep attachment web/src` hors code généré rend zéro résultat — donc la route existe, le client généré la connaît, et aucune vue ne l'appelle. C'est une surface visuelle, donc la même question que D21 pose, et elle est nommée plutôt que bâclée.

**M4vicies en détail.** Ce que la base n'avait jamais vu.

**Le trou que M4novodecies ne pouvait pas atteindre.** Cette purge-là collecte les pièces jointes dont la *notification* a expiré, ce qui suppose qu'une révision les ait référencées au moins une fois. Presigner, téléverser 25 MiB, ne jamais publier : aucune ligne de ce schéma n'a jamais porté cet identifiant, donc aucune requête ne peut le nommer. C'est le trou qu'un abus viserait, et le seul remède est de **noter la signature au moment où elle est faite**.

**Une table, et la discipline qui va avec.** `attachment_presigns` porte `(id, channel_id, requested_at)`. Le `channel_id` n'est pas décoratif : c'est la première question d'un opérateur devant un bucket qui grossit — qui a demandé ces objets —, et le canal est ce qui a autorisé la signature (§6.1) même si l'attachment n'y est pas lié.

**Ce qu'une ligne veut dire, dit précisément** : « une URL a été signée pour cet identifiant », jamais « un objet existe ». Rien ne nous dit si le client a téléversé, et rien n'a besoin de le savoir : supprimer une clé jamais écrite **réussit** sur S3. Le balayage sur des identifiants dont l'upload n'a jamais eu lieu est donc un non-événement, ce qu'il faut avoir en tête en lisant les compteurs plutôt que de le découvrir en s'étonnant d'un nombre.

**Le délai de grâce est la seule chose qui empêche ce job de causer un incident.** Entre la signature d'une URL et la publication de la notification qui la nomme, un identifiant est **légitimement** non référencé ; un balayage qui tombe dans cette fenêtre supprime un upload en cours de route. Un jour : l'URL présignée vit quinze minutes, donc c'est deux ordres de grandeur au-dessus du besoin, et ça borne quand même le cas d'abus.

**La ligne part avec l'objet, jamais avant.** Un identifiant dont la suppression échoue garde sa ligne et revient à la passe suivante — même forme auto-réparatrice que le balayage voisin, et pour la même raison : la ligne est ce qui rend l'objet trouvable.

**Deux balayages, et aucun ne remplace l'autre**, ce qui méritait d'être tranché plutôt que subi. Le registre couvre en apparence les deux cas — un objet dont la notification a été purgée finit lui aussi sans référence — mais les deux mécanismes n'échouent pas pareil : celui de M4novodecies **ne dépend d'aucun registre** et survit donc à une ligne perdue (une écriture ratée après la signature, une restauration partielle), tandis que celui-ci atteint des identifiants qu'aucune révision n'a jamais nommés. Deux questions différentes — « la dernière référence de cet objet s'en va » et « cet objet en a-t-il jamais eu une » — donc deux compteurs distincts dans le rapport, parce qu'un opérateur qui voit un bucket grossir veut savoir lequel bouge : `presigns_collected` élevé, ce sont des publishers qui presignent sans publier, c'est-à-dire un bug client ou un abus, pas de la rétention.

**Une migration avec un backfill**, parce qu'une instance qui a déjà des données ne doit pas porter une génération d'objets définitivement incollectables : le `INSERT ... SELECT DISTINCT ON` recopie les identifiants que les révisions existantes référencent. Ce qu'il ne peut **pas** rattraper est écrit dans le fichier : un upload jamais référencé *avant* la migration est exactement ce que rien n'a enregistré, et il reste perdu pour le balayage. Le `requested_at` du backfill vient de la révision qui l'a nommé — plus tard que la vraie signature, donc l'erreur penche vers garder l'objet plutôt que le collecter trop tôt.

**Vérifiée, pas seulement écrite** : T24 (« premier boot construit le schéma, second ne rejoue rien ») passe avec la nouvelle migration, donc le schéma **et son backfill** s'exécutent sur une base créée pour l'occasion ; les 64 tests e2e passent contre le Postgres de dev.

**Un type qui est apparu en cours de route, et qui est mieux que ce qu'il remplace.** `PurgeExpired::new` a franchi les sept arguments et clippy l'a signalé. Plutôt qu'un `allow`, le bucket et le registre sont devenus **un** paramètre, `Attachments { store, registry }` — regroupement qui a un sens et non un contournement : *tout* appelant a besoin des deux (signer écrit dans l'un et enregistre dans l'autre ; collecter lit l'un pour décider quoi supprimer de l'autre), et un appelant qui ne tiendrait que le bucket est un appelant capable de créer des objets que rien ne collectera — précisément le bug que cette paire existe pour empêcher.

**Cinq tests** : l'upload jamais publié qui est collecté et dont la ligne disparaît, celui encore dans sa grâce qu'on ne touche pas, celui qui a été publié et qui n'est jamais vu comme non référencé, la signature enregistrée, et le refus qui n'enregistre rien. Plus deux au niveau de l'adapter, contre la vraie base.

**M4novodecies en détail.** Une méthode de port dont la documentation nommait son appelant, et l'appelant n'existait pas.

**Ce que le diff de §7.2bis a donné.** La section promet une purge périodique soft-delete → hard-delete, leader-only. Tout y est et tout tourne : le use case `PurgeExpired`, le verrou, la reprise entre lots, la boucle câblée sur un timer dans `main.rs`, le compteur « N expirées pendant votre absence », la compaction. Rien à redire — jusqu'à la ligne d'à côté.

`AttachmentStore::delete` porte ce commentaire : « Delete an object, when its notification is purged. » Un grep sur ses appelants en rend **un** : le remplacement d'avatar. La purge ne l'appelle pas, n'a même pas le port dans ses collaborateurs. Donc **chaque pièce jointe jamais publiée survivait à sa notification, définitivement** — 25 MiB pièce (§8.2), sur un chemin que tout publisher peut emprunter (presign, §6.1). Ce n'est pas un écart de documentation, c'est une croissance de stockage sans borne, et la seule raison qu'elle n'ait pas été vue est qu'aucun test ne peut échouer sur un octet qui reste.

À sa décharge, §7.2bis ne demandait rien : elle dit que « les révisions d'une notification expirent avec elle » et se tait sur les objets. La narration de D14 avait frôlé le sujet — « l'attachment n'est pas *lié* au canal, le lier voudrait dire décider ce qu'il devient quand le canal est archivé, ce que §7.2bis ne demande pas » — en tranchant la question du canal et en laissant celle de la notification. **§7.2bis est donc modifiée par cet incrément**, pas seulement honorée : c'est le premier de la série où le document avait un trou plutôt que le code.

**L'ordre est la conception.** Les objets partent **avant** les lignes qui les nomment. Les lignes sont le seul endroit qui sait quel objet appartient à quoi : les supprimer d'abord laisserait le bucket plein de fichiers que plus rien ne peut nommer, et rien de moins qu'un listing complet du bucket ne les retrouverait. Dans cet ordre, un échec est bon marché — le bucket injoignable laisse les lignes en place, la passe suivante repose la même question et obtient la même réponse. L'ordre inverse aurait exactement une chance.

Corollaire assumé : **si un objet ne part pas, les lignes ne partent pas non plus**, sinon on effacerait le seul enregistrement de ce qui reste à supprimer. Le risque est nommé plutôt que découvert — un objet en échec *permanent* bloque la purge des lignes, parce que le lot est pris par `deleted_at` et que le même revient à chaque passe. Ça se voit (`purged: 0` et un warning par tick) et ça se répare par un opérateur. C'est jugé le meilleur des deux risques parce que **supprimer une clé inexistante réussit** sur S3 : la panne réaliste ici est un bucket injoignable, transitoire et global, pas un fichier coincé. Le pruning des events et des réservations d'idempotence, lui, n'est **pas** conditionné à la balayette — ils ne possèdent aucun objet, et laisser une panne de stockage arrêter la purge du log ferait d'un problème de disque un problème de base.

**« Seulement » est toute la sûreté.** Un identifiant référencé par une révision hors du lot est laissé tranquille. Rien n'interdit à un publisher de nommer la même pièce jointe dans deux notifications, et supprimer l'image d'une notification vivante parce qu'une expirée sans rapport la désignait serait une perte de données **causée par le ménage** — pire que la fuite qu'il nettoie. C'est un `NOT EXISTS` et non un `NOT IN`, pour une raison qui vaut d'être écrite : la sous-requête sélectionne une extraction JSON qui vaut `NULL` pour toute révision sans image, et `NOT IN` sur un ensemble contenant `NULL` n'est jamais vrai — la requête ne rendrait **rien, pour toujours**, et ressemblerait exactement à un bucket propre.

**Vérifié contre une vraie base**, ce qui est le point le plus important de cet incrément. Un chemin JSON `content->'image'->>'ref'` compile en Rust et peut ne rien matcher en Postgres ; l'écrire sans l'exécuter aurait produit précisément le genre de surface non vérifiée que ce document refuse ailleurs (D13, D17). Le Postgres de dev **tourne dans ce workspace** — la suite `e2e` se sautait simplement faute de `WNK_TEST_DATABASE_URL` dans l'environnement, ce qui est son comportement voulu (« verte sur une machine qui n'a pas démarré la pile ») et qui, ici, cachait qu'elle était disponible. Les 62 tests e2e passent réellement, et les deux nouveaux avec.

**Cinq tests**, dont un **falsifié** : la clause `NOT EXISTS` retirée, le test de la pièce jointe partagée échoue contre la vraie base sur « deleting it would be data loss ». Trois au niveau du use case (l'objet part avec sa notification, une notification vivante garde le sien, une notification sans image ne balaye rien mais purge quand même ses lignes) et deux au niveau de l'adapter, dont un qui garde le piège du `NOT IN`.

**Ce qui reste, et est ouvert en D25** : un upload presigné **jamais référencé**. La base ne l'a jamais vu, donc aucune requête ne peut le nommer, et c'est le trou qu'un abus viserait — presigner, uploader 25 MiB, ne jamais publier. Il demande un registre des presigns, donc une table et une migration, et une notion de délai de grâce pour ne pas supprimer un upload en vol : un incrément à part entière plutôt qu'un ajout discret à celui-ci.

**M4octodecies en détail.** La phrase au-dessus du tableau, cette fois.

**Où elle était.** M4septdecies a diffé les dix **lignes** de §8.2 et en a trouvé une non appliquée. La **phrase d'en-tête** du même tableau n'avait pas été diffée : « Défauts globaux, **surchargeables par canal** (`limits` dans `[[channels.managed]]`, §11) **et par API key**. » Deux promesses de plus, dans la même section, à une ligne de distance de celle qu'on venait de solder.

**Ce que le code disait.** `ChannelLimits { publish_per_min }` existe dans `config.rs` depuis que §11 y a été modélisé — parsé, validé de nom, et **lu par personne** : un `rtk grep` sur le type rendait deux résultats, sa déclaration et son champ. La valeur traversait la configuration et s'arrêtait là.

Le détail qui rend celui-ci différent des précédents : ce dépôt a un dispositif **exactement pour ça**. `Config::unhonoured()` liste au démarrage les sections qu'un build parse sans les honorer — « une clé de configuration silencieusement ignorée, c'est un après-midi perdu pour un opérateur », dit son module. Il nomme `[[trust_policy]]` sans `[federation]`, les publishers sans `[federation]`, `[push.apns]` et `[push.unifiedpush]`. Il ne nommait **pas** `limits`. La seule clé réellement tombée était celle que le détecteur de clés tombées ne connaissait pas — ce qui est le mode d'échec d'un filet dont les mailles sont écrites à la main, et vaut d'être noté à côté du filet.

**Ce qui a été câblé**, et ce qui ne l'a pas été. `publish_per_minute: Option<u32>` sur `ChannelPolicy` ; le réconciliateur y met `limits.publish_per_min` ; `check_quotas` prend le **canal chargé** au lieu de son id — les deux appelants l'avaient déjà en main après l'autorisation, donc l'override ne coûte aucune lecture de plus et n'a rien réordonné ; l'adapter mesure contre `configured.unwrap_or(défaut)`.

**Aucune migration** : la policy est persistée en JSON, donc un `#[serde(default)]` suffit et une policy écrite avant ce champ se relit. Le fait valait d'être vérifié avant de concevoir — la même feature contre des colonnes aurait été un incrément d'une autre taille.

**Le ceiling n'est pas un champ de `Quota`**, et c'est la décision qui méritait d'être prise plutôt que subie. `Quota` répond « qui est compté » : c'est la clé de ligne en base, la clé de map, et ce qui est comparé par égalité. Y glisser un plafond ferait de deux requêtes sur le même canal deux sujets différents dès qu'un opérateur édite le nombre. Le plafond voyage donc en argument séparé, `None` voulant dire « le défaut publié ».

**Rien n'est clampé au défaut, dans aucun sens.** Un canal peut recevoir plus de place que 600 comme moins : qui écrit `publish_per_min = 5000` en GitOps l'a dit, et une limite qui ignore silencieusement le nombre qu'on lui donne est pire qu'une limite absente. Le seul refus est `0`, **au démarrage** : il bloquerait chaque publication du canal avec un `429` qui ressemble à de la charge et pas à de la configuration, alors que qui l'écrit veut dire « sans limite » — et la façon de demander ça est d'omettre la clé.

**GitOps seulement, et pas dans le `PATCH`.** §8.2 accroche `limits` à `[[channels.managed]]`, donc l'override n'entre pas dans le corps de `PATCH /channels/{id}` : un publisher qui pourrait relever le plafond de son propre canal par l'API est un publisher pour qui le plafond est une suggestion. Un canal dynamique n'a pas de fichier où être nommé et garde le défaut, ce qui est la bonne réponse et non un trou.

**Six tests**, dont deux vérifiés par falsification : le réconciliateur qui laisse tomber la valeur fait échouer le test qui la suit jusqu'à la policy. Les deux tests du use case assertent **au port** — que le nombre du canal arrive au limiteur, et que la clé n'en reçoit jamais — parce que l'application vit dans l'adapter qui tient le compteur ; ce que le use case doit, c'est ne pas perdre le nombre en route.

**Ce qui reste, et est ouvert en D24** : la moitié « **et par API key** » de la même phrase. Rien nulle part — ni champ sur `ApiKey`, ni surface pour le déclarer — et elle ne se câble pas comme l'autre, parce qu'elle pose une question que §8.2 et §5.2 ne tranchent pas : **qui a le droit de fixer le plafond d'une clé ?** Une clé créée par son propre porteur avec son propre plafond ne serait plus un plafond. Écrite plutôt que devinée.

**M4septdecies en détail.** Une ligne de tableau, publiée et jamais lue.

**Comment elle a été trouvée.** §5 et §6 diffés contre le code — les deux dernières grosses sections dont les promesses n'avaient pas été comparées ligne à ligne. Elles sont **propres**, et c'est un résultat qui vaut d'être écrit plutôt que passé : les quatre principals de §5.1 sortent d'un seul middleware, le durcissement de l'échange CI de §5.3 (`aud`, glob ancré, `jti` mémorisé) est là avec ses tests, `platform-admin` n'est atteignable que par un user OIDC et ne se lit nulle part ailleurs que dans les claims (§5.4), la règle §5.2bis « un bot n'est jamais compté dans les receipts » est portée par `Readership::of` avec son test, et la révocation de device efface bien subscription, curseur **et** clé publique jusque dans l'adapter Postgres (§6.2, §15). Rien à corriger dans les deux sections.

Le trou était à côté, dans **§8.2** : dix limites en tableau, neuf enforcées, une non. « Connexions SSE/WS par user | 20 | multi-onglets + devices » n'existait **nulle part** — ni constante, ni port, ni contrôle : `GET /stream` et `GET /ws` ouvraient autant de connexions qu'on en demandait. C'est exactement la trouvaille de M4duodecies (`MAX_PAYLOAD_BYTES`, une constante que rien ne lisait) au degré au-dessus : ici il n'y avait même pas la constante. Et les deux modules qui auraient dû l'attraper l'affirmaient à l'envers — `limits.rs` s'ouvre sur « toutes les limites que le domaine applique, au même endroit », `Quota` sur « chaque limite ici figure en §8.2 ». Les deux disent vrai dans un sens et aucun ne vérifie l'autre : une limite de §8.2 absente des deux ne fait échouer personne.

**Pourquoi ce n'est pas le port `RateLimiter`.** Il compte des *requêtes sur une fenêtre*, ce que sont les neuf autres lignes. Celle-ci borne un **nombre tenu simultanément** : consommer un jeton à la connexion plafonnerait les *tentatives par minute*, et un client qui se reconnecte toute la journée dans son budget accumulerait quand même des sockets sans fin — précisément la panne à éviter. Question différente, mécanisme différent, dit à l'endroit où quelqu'un aura le réflexe inverse.

**Ce qui tient le compte** : une `HashMap<Principal, usize>` dans le process, et un garde qui rend la place **au `Drop`**. Le `Drop` n'est pas un détail d'implémentation, c'est tout le dispositif : un flux se termine quand le client part, quand la tâche est annulée à l'arrêt, ou quand le log devient illisible, et une libération posée sur le chemin de succès d'un handler fuirait une place à chacun des trois. Une place fuitée est **pire que pas de plafond** — c'est un user enfermé dehors de sa propre inbox jusqu'au redémarrage du replica. Le garde des métriques de §13 vivait déjà à côté avec la même forme et la même raison ; les deux sont maintenant **un seul** garde, parce qu'ils ont exactement la même durée de vie et que deux décréments sont deux occasions de se tromper.

**Par principal et non par user**, ce qui élargit §8.2 volontairement : le texte dit « user » parce qu'il a été écrit pour la personne à onglets, mais un service account tient des connexions lui aussi (§5.2bis) et un bot en boucle de reconnexion est la même fuite avec moins d'avertissement.

**Ce que les tests tiennent, et comment on sait qu'ils tiennent quelque chose.** Six au niveau du registre (le plafond, la place rendue, l'isolation entre principals, le plafond propre du service account, aucune ligne laissée derrière un partant, un refus qui ne compte rien) et deux au niveau HTTP (le 21ᵉ reçoit un `429` avec son `Retry-After`, et le WebSocket partage le plafond du SSE au lieu d'en avoir un second — quarante sinon). Les deux tests HTTP ont été **vérifiés par falsification** avant d'être gardés : enforcement désactivé, ils échouent tous les deux. Un test de plafond qui passerait aussi sans le plafond ne teste rien, et c'est le mode d'échec que ce projet a déjà nommé deux fois (D8, la matrice OIDC de §15).

**Ce que le `Retry-After` vaut, dit honnêtement** : 30 s, advisory. Contrairement à une fenêtre de rate limit il n'est pas calculé — une place se libère quand quelqu'un ferme un onglet, et rien ici ne sait quand. Il est là parce qu'un `429` sans `Retry-After` transforme un client poli en boucle d'attente active, ce qu'un test de `problem.rs` disait déjà.

**Ce qui reste ouvert, et est ouvert en D23** : le compte est **par replica**. Avec plusieurs replicas derrière un load balancer, un principal peut en tenir `20 × replicas`. Écrit plutôt qu'arrondi — mais l'écart est plus petit qu'il n'en a l'air, parce que ce que le plafond défend est la mémoire, la tâche et le descripteur **du process qui tient la connexion** : un plafond par replica borne exactement ça, sur chaque replica. C'est la différence avec le rate limit de publication, dont le port documente le choix inverse pour une bonne raison — celui-là protège un aval **partagé** (le fan-out, la base), donc un compteur en process y appliquerait `n × 60/min` contre une ressource qui n'a de la place que pour `60/min`. Deux limites qui se ressemblent dans un tableau et ne défendent pas la même chose.

**M4sedecies en détail.** La dernière copie, et ce qui reste local pour de bonnes raisons.

**Ce que le web faisait encore.** `stores/preferences.ts` portait `muted(dnd, channel, priority, at)` : les quatre clauses de §7.4, réécrites en TypeScript, avec leurs propres tests — fenêtre horaire dans son fuseau, exception qui sonne quand même, portée par canal, `urgent` qui ne passe que sur autorisation. La même règle que le domaine applique au push et que la CLI vient de perdre. Elle lit maintenant `muted_by_dnd` sur l'événement, comme la CLI.

**Cinquante lignes de moins, et cinq tests de moins**, ce qui est la bonne direction pour une règle de sécurité : les tests supprimés ne testaient pas un comportement du produit mais une **réimplémentation** de ce comportement, et ce sont eux qui donnaient l'illusion que la copie était sûre. Ce qu'ils tenaient est tenu par `notify_domain::Dnd::is_muted` et par les deux tests de transport qui vérifient que la réponse voyage.

**Ce qui reste local, et pourquoi ce n'est pas la même dette.** Le `silence` de §2.3 continue d'être décidé par le client : il porte sur une notification ou un `group_key`, le client détient déjà la liste complète par l'événement `preference`, et rien n'est à demander à personne pour y répondre. Ce n'est pas une règle dupliquée — c'est une donnée locale lue localement. La distinction vaut d'être posée, parce que « le client ne décide plus rien » aurait été faux, et une phrase fausse dans un document est ce que D22 vient de coûter trois fois.

**Un cas que le drapeau ne couvre pas, nommé** : une notification remise par le service worker n'a pas d'enveloppe d'événement, donc pas de drapeau. `undefined` y veut dire « personne n'a rien dit », ce qui n'est pas « muet » — bruyant quand on ne sait pas, parce qu'une popup en trop est une gêne et une popup manquante est un incident manqué. Un test le tient.

**M4quindecies en détail.** La réponse était déjà calculée et jetée.

**Ce que le diff a montré avant d'écrire quoi que ce soit.** `SyncBacklog::execute` remplit `deferred_by_dnd` — la liste, par appelant, des notifications qu'un DND a retenues (§7.4) — et `Connection::drain`, dans le flux SSE et WebSocket, gardait `page.events` et laissait tomber la liste. Le serveur répondait donc déjà à la question « est-ce que ça doit faire du bruit pour *ce* lecteur », une fois par page, et chaque client la reposait pour lui-même. C'est la forme la moins chère possible de D22 : il n'y avait rien à calculer, seulement à transporter.

**Un fait de livraison, pas une colonne.** `muted_by_dnd` est sur l'enveloppe de l'événement, à côté de `cursor` et `logged_at` qui sont aussi des faits de livraison. Le **même** événement est `false` pour un lecteur et `true` pour un autre, donc il n'est jamais stocké : les trois endroits qui construisent un `Event` vers la base ou vers un sink l'écrivent `false` avec la raison en commentaire, et les deux transports le posent à la sortie.

**Absent quand il est faux**, et c'est le contrat : le champ est `skip_serializing_if`, donc une trame qui ne mute rien est octet pour octet celle d'avant, et un client écrit avant ce champ continue de parser. Le schéma manuel de §8.1 (D1) le déclare **optionnel** pour la même raison. Deux tests le tiennent, un par sens — présent et `true` sous une préférence active, absent sinon.

**Les deux chemins doivent s'accorder**, et un test le dit : `/sync` porte la liste `deferred_by_dnd` *et* le drapeau sur chaque événement, le flux porte le drapeau ; un client qui rattrape par l'un et un client connecté par l'autre doivent conclure la même chose de la même notification. La liste reste : c'est elle que §7.2 lit pour ne pas badger « reçue en retard » un retard demandé, et le drapeau est la décision de popup. Deux consommateurs, deux formes, une source.

**Ce que ça supprime.** `notify-cli/src/tui/desktop.rs` perdait cent vingt lignes : la fenêtre horaire dans son propre fuseau, la clause de minuit dont la queue appartient à la veille, la portée par canal, le bypass `urgent`, et les neuf tests qui les tenaient — tout cela était une deuxième implémentation d'une règle que le serveur applique déjà. Il reste l'envoi au démon, qui est ce que ce fichier devait être. La lecture du DND au rafraîchissement disparaît avec : une requête de moins par `r`.

**Ce qui reste, nommé.** Le web garde sa copie en TypeScript : il peut lire le même drapeau, c'est le même événement, mais le toucher voudrait dire retoucher une interface que personne ici ne peut regarder (D21). Et le drapeau ne couvre que le **DND** — un `silence` actif (§2.3) est l'autre moitié de « ne me préviens pas », toujours décidée côté client. La suite de D22 est là, et elle est plus petite que ce qui vient d'être fait.

**M4quaterdecies en détail.** Le relais desktop, et une clause de §9.2 retirée plutôt que sous-livrée.

**La clause impossible d'abord.** §9.2 promettait un relais « quand le terminal n'a pas le focus ». Il n'existe aucune façon portable de savoir ça : X11, Wayland, macOS et Windows répondent différemment ou pas du tout, et un multiplexeur casse la réponse qu'on finit par choisir. Les deux issues honnêtes étaient un heuristique par plateforme — faux chez quelqu'un, indéboguable d'ici — ou le retrait. §9.2 est **amendé** : le drapeau relaie quand il est allumé, et c'est à la personne de l'éteindre.

**La règle de §7.4, elle, est tenue à la lettre.** Le flux continue pendant un DND — une app ouverte est un choix conscient de regarder — donc le serveur envoie et le client décide. Les quatre clauses sont celles du serveur, dans le même ordre : une exception sonne toujours (le cas astreinte), hors de portée ça sonne, une préférence qui existe n'est pas une préférence qui tourne, et `urgent` ne passe que si le destinataire l'a autorisé. Neuf tests, dont celui qui vaut le plus : **la queue d'une nuit appartient à la veille** — une fenêtre lundi-vendredi 22:00–08:00 doit rester silencieuse à trois heures du matin le samedi. C'est la clause qu'une réimplémentation rate, et c'est pourquoi elle est copiée du domaine plutôt que redérivée.

**Ce qui compte comme une arrivée** est une question à part, et elle a ses tests : l'identité est la notification, pas la révision — un incident amendé vingt fois est une popup, pas vingt — et le **premier chargement n'est pas une arrivée**, sinon ouvrir le TUI ferait apparaître l'inbox entière sur le bureau. Le drain se fait dans la boucle, jamais dans `update`, pour la raison qui vaut déjà pour le signal local : `update` change l'état et reste sans I/O.

**Deux limites nommées.** La préférence est relue au rafraîchissement, pas en direct : §7.4 promet une propagation temps réel et ce client la prend au prochain `r`. Et la priorité n'est pas dans le cache local, donc une arrivée est traitée comme `default` — ce qui veut dire qu'une notification `urgent` est **supprimée** pendant une nuit même si le destinataire a autorisé le bypass. Se tromper dans ce sens est le bon sens : le silence promis est tenu, et la notification est dans l'inbox.

**Ce que la livraison ne prouve pas** : que le démon reçoit quoi que ce soit. Il n'y a ni session desktop ni D-Bus ici. La ligne qui appelle `notify-rust` est écrite et n'a jamais été exécutée — D19 reste en jaune, comme D21, et pour la même raison.

### 14.2 Accès par groupe OIDC — le modèle dérivé

Découvert en câblant M1.8 : `VerifiedClaims.groups` était extrait du token et **jeté**. `Authenticate::bearer` construisait `Caller::member(principal)` et `Authorizer` ne résolvait que par `Membership` stocké, donc tous les bindings `[[channels.managed.access]] oidc_group` de §11 n'accordaient rien — alice se connectait et ne voyait aucun canal.

Deux lectures étaient possibles, et §5.1 tranche :

- **Matérialiser** les groupes en memberships au login. Écarté : §5.1 impose « zéro session serveur » et l'auth par bearer se rejoue à *chaque requête*, donc ce serait une écriture par requête — et un membership matérialisé devient périmé dès que quelqu'un quitte un groupe.
- **Dériver** : les groupes voyagent sur le `Caller`, le canal porte ses bindings de groupe, et l'`Authorizer` consulte les deux portes. C'est ce que dit §5.1 : « valide en pur JWKS, **mappe `groups` → rôles**, ne stocke aucune session ».

Le modèle dérivé est implémenté, et il traverse cinq crates :

| Couche | Changement |
|---|---|
| `notify-domain` | `GroupBinding` sur le `Channel` ; `Channel::allows` réunit les deux portes en un appel |
| `notify-ports` | `subscribed_ids` et `list_for` prennent les groupes ; `SyncQuery` aussi |
| `notify-persistence-pg` | table `channel_group_access` + un prédicat `EXISTS`/`EXISTS` partagé par les 3 requêtes de portée — un `JOIN` rendrait le canal deux fois à qui est à la fois membre et dans un groupe lié, et le curseur de pagination sauterait une ligne |
| `notify-app` | `Caller` porte les groupes du token ; une API key n'en reçoit aucun (elle a ses propres scopes, §5.2) |
| `notify-server` | le réconciliateur écrit les bindings depuis la config |

Propriété conservée pour T29 : une liste de groupes vide n'accorde **rien**. Un émetteur sans claim `groups` dégrade vers les bindings individuels et jamais vers un accès par défaut.

### 14.3 Ports sans adapter de production

Recensé en câblant M1.7 et M1.8. Un port dont **aucun** adapter n'existe est un `Arc<dyn …>` que la composition root ne peut pas fournir.

| Port | Double in-memory | Adapter de production | Conséquence |
|---|---|---|---|
| `AuditLog` | ✅ | ✅ `PgStore` (M1.8) | Vocabulaire d'actions fermé — une valeur inconnue en base est signalée, jamais renommée en silence. |
| `PushDispatcher` | ✅ | ✅ `TransportRegistry` (M1.8) + `WebPush` (M3.5) | Le registre reste vide sur un déploiement sans `[push.webpush]`, et c'est le comportement correct : `Fanout` saute déjà les devices `stream`, donc il n'est atteint que par un device enrôlé sur un transport que cette instance ne sert pas — il répond `NotConfigured` en nommant la capacité. Il dit aussi ce que ses transports portent, et c'est ce plafond qui décide charge utile ou poke (M3.5). |
| `AttachmentStore` | ✅ | ✅ `S3Attachments` (M2.2) | Conformité passée contre un vrai RustFS. Un déploiement sans `[attachments]` reçoit un double qui **refuse** (`NotConfigured`) au lieu de réussir dans le vide. |
| `AttachmentRegistry` | ✅ | ✅ `PgStore` (M4vicies) | Le pendant du précédent, et volontairement un port **séparé** : celui-là est un bucket, celui-ci est la base qui se souvient de ce qu'on a signé (D25). Les fondre mettrait une écriture SQL derrière un trait dont l'autre implémentation parle S3. |
| `AvatarProcessor` | — | ✅ `notify-avatars` (M2.2) | Seule surface de décodage média du serveur (§15) : taille, format d'après les octets, dimensions **avant** décodage, ré-encodage qui strippe les métadonnées. |
| `SinkDispatcher` | ✅ `RecordingSinkDispatcher` | ✅ `WebhookSink` + `EmailSink` (M4bis) | Une flotte par genre, comme `TransportRegistry` : un genre sans adapter nomme sa capacité (`sinks.webhook`) plutôt que d'échouer vaguement. Le classement retry/rejet vit là — seul ce qui a parlé HTTP sait qu'un `410` ne changera pas d'avis. |
| `SinkQueue` | ✅ | ✅ `PgStore` (M4bis) | Prendre est un **bail** et non un retrait : un leader tué en pleine tentative laisse la livraison due plutôt que perdue. La ligne nomme son sink et ne porte **jamais** son secret (ADR-0018). |
| `SinkRegistry` | ✅ `StaticSinkRegistry` | ✅ `SinkRoutes` (M4bis) | Synchrone et construit depuis la config : un `async` ici inviterait une implémentation qui va *lire* quelque chose, c'est-à-dire un sink éditable par l'API dans le dos de §11. |
| `FederationVerifier` | ✅ `StaticFederationVerifier` (M4.1) | ✅ `FederationTokens` (M4.2) | Un `OidcVerifier` par forge, donc tout le durcissement JWKS de §5.1 hérité ; l'allowlist d'issuers vient des trust policies et **est** la protection anti-SSRF d'un endpoint sans credential. Le port rend des `FederatedClaims` — il rendait des `VerifiedClaims`, qui ne portent ni `aud` ni `jti`, donc les deux contrôles que sa doc promettait étaient **irreprésentables** (M4.1). |
| `ReplayGuard` | ✅ `InMemoryReplayGuard` (M4.1) | ✅ `PgStore` (M4.2) | Un `INSERT … ON CONFLICT DO NOTHING` : « lire puis écrire » est une course que deux jobs gagnent tous les deux. Port à part et non `IdempotencyStore`, qui **rejoue** la réponse stockée — ce qui rendrait une seconde copie du jeton à qui rejoue un ID token volé. |
| `Metrics` | ✅ `NoMetrics` | ✅ `PrometheusMetrics` (M4ter) | Un port plutôt qu'un enregistreur global : la racine de composition décide si les nombres vont dans un registre ou nulle part, et rien au-dessus ne peut le dire. `NoMetrics` n'est pas un manque — une instance qui ne mesure rien sert un document **vide**, ce qui est une réponse de configuration et pas une erreur. |
| `TokenVerifier` | ✅ | ✅ `notify-oidc` | |
| `SecretHasher` | ✅ | ✅ `notify-oidc` (Argon2id) | |
| `KeyGenerator` | ✅ | ✅ `UlidKeyGenerator` (`notify-app`) | Dérive préfixe et secret de ULID — 160 bits d'aléa CSPRNG. |
| `Clock` | ✅ | ✅ `SystemClock` (`notify-ports`) | |
| Tous les dépôts, `EventStore`, `IdempotencyStore`, `RateLimiter` | ✅ | ✅ `PgStore` | |
| `LeaderElection` | ✅ | ✅ `PgLeaderElection` (advisory lock) | Backend `kubernetes` (Lease API, §11) non implémenté — c'est une option de config, pas un manque. |
| `EventBus` | ✅ | ✅ `PgEventBus` | `eventbus-redis` vide — option d'échelle (§11), pas un manque. |

### 14.4 Dette identifiée, et comment la solder

Tout ce qui suit est connu, écrit et chiffré en effort. Une dette sans solution écrite est un souvenir : au bout de trois mois plus personne ne sait si c'était un choix ou un oubli. Chaque entrée dit donc **ce que ça coûte aujourd'hui** et **par quoi ça se répare**, dans l'ordre où il vaut mieux les prendre.

| # | Dette | Coût aujourd'hui | Effort | État |
|---|---|---|---|---|
| D1 | `Event` illisible par le client Rust | — | M | ✅ soldée en fermant M2 |
| D2 | `PATCH /channels/{id}` absent | — | S | ✅ soldée en fermant M2 |
| D3 | Items paginés inlinés dans la spec | — | S | ✅ soldée en fermant M2 |
| D4 | Commandes `:` non exécutées | — | M | ✅ soldée en fermant M2 |
| D5 | Signal local absent (§9.3bis) | — | M | ✅ soldée en fermant M2 |
| D6 | `delivered_at` non conservé | — | S | ✅ soldée en fermant M2 |
| D7 | Legs Dex et Authentik de `oidc-compat` | — | M | ✅ **soldée par M4quatervicies — et la jambe était rouge**. Dex démarré pour la première fois, la matrice a échoué aussitôt : elle affirmait `sub == username` partout, ce que seul un fournisseur arrangé pour ça fait. L'assertion est devenue ce dont le code dépend — un `sub` non vide et **stable entre deux connexions** — et les deux jambes passent. Authentik reste en compose/CI par choix (§12.1) |
| D8 | Une vérification qui passe sans rien exécuter | — | S | ✅ règle appliquée |
| D9 | Refresh token en `sessionStorage` (ADR-0016) | lisible par un XSS aussi longtemps qu'il vit | M | ✅ **tranchée — risque accepté** (ADR-0020, option A) : le jeton ne bouge pas, les quatre atténuations et les conditions de réouverture sont écrites |
| D10 | `task test:cover:domain` mesurait autre chose que son gate | — | S | ✅ soldée en fermant M3 |
| D11 | `--action` et `--image` promis par §9.1, absents de la CLI | — | S | ✅ soldée |
| D12 | Le workflow de dogfooding n'a jamais tourné contre une vraie forge | deux lignes non observées, toutes deux bruyantes si fausses (M4.7) | S | ⬜ ouverte |
| D13 | L'image de dev ne porte pas la pile GTK/WebKit | M5 ne peut pas être compilé, encore moins lancé (§12.4) — la coquille de `web/src-tauri` existe depuis M4tricies et n'est bâtie par rien | S | ✅ **soldée — `che-mise-webkit` remplace `che-min-mise` comme parent du devfile**, `task tauri:doctor` rapporte les quatre bibliothèques présentes, et le premier build jamais tenté de `web/src-tauri` a réussi — détail ci-dessous |
| D14 | `POST /attachments/presign` listé par §8 et servi par aucun build | — | M | ✅ soldée |
| D15 | `sink_dlq_depth` (§13) sans exporteur Prometheus pour la porter | la profondeur de DLQ ne se lit que dans la console, jamais dans une alerte | M | ✅ soldée par M4ter |
| D16 | Les traces de §13 ne sont pas câblées | un incident se reconstitue à partir de logs corrélés à la main, pas d'une trace | M | ✅ soldée par M4quater |
| D17 | Rien ne sert le bundle : pas de Containerfile, donc pas d'en-têtes | `frame-ancestors` et `report-to` absents ; l'étape image de la release est un `::warning::` | M | 🟡 **écrite, jamais construite** (M4tervicies) — les deux Containerfiles existent, les trois en-têtes ont un émetteur, `release.yaml` construit/signe/scanne au lieu d'avertir, et `trivy config` les valide **ici** ; aucune image n'a jamais été bâtie, la première fois est sur le runner |
| D18 | `GET /channels/{id}` listé par §8 et servi par aucun build | un client voulant un canal paginait `GET /channels` jusqu'à le trouver | S | ✅ soldée par M4sexies — la description du détail promet encore les sinks, qui ne sont pas servis (ci-dessous) |
| D19 | `--notify-send` (§9.2) n'existe pas | — | S | ✅ **soldée par M4sexvicies — vue, et le sidecar aidait après tout**. Pas le navigateur : son *image*, qui porte un `dbus-daemon` que rien d'autre ici n'a. `task desktop:bus` ouvre la socket sur le chemin que les deux conteneurs montent, `xtask notifyd` répond aux quatre méthodes de `org.freedesktop.Notifications`, et `task desktop:check` prouve la chaîne. Le vrai défaut n'était pas l'environnement : `relay()` avalait ses erreurs, donc rien ne pouvait la prouver — elle est coupée en deux, le silence conservé là où il était voulu |
| D20 | Mode `:` sans complétion ni historique (§9.2) | la parité CLI existe, la découvrabilité qu'elle promettait non | S | ✅ soldée par M4undecies |
| D21 | `GET /search` n'est appelé par aucun client ; pas de palette `Cmd+K` (§10.4) | — | M | ✅ **soldée par M4quatervicies — vue, et trois défauts corrigés** : curseur clavier et survol souris rendaient le même gris (`Entrée` était un pari), le passage à la dernière ligne sélectionnait hors écran, et un lecteur d'écran n'apprenait rien. Aucun des trois n'est visible sous jsdom |
| D22 | La règle DND de §7.4 existe en trois exemplaires | domaine (Rust), client CLI (Rust), web (TypeScript) — trois occasions de diverger | M | ✅ soldée — M4quindecies côté serveur et CLI, M4sedecies côté web : il n'en reste **qu'une**, celle du domaine |
| D23 | Le plafond de connexions de §8.2 est compté **par replica** | un principal peut tenir `20 × replicas` sur l'instance, là où §8.2 se lit « 20 » | M | ⬜ ouverte — ouverte par l'incrément qui a posé le plafond (M4septdecies) ; ce qui est garanti, et ce que ça coûte vraiment, ci-dessous |
| D24 | §8.2 promet des limites surchargeables **par API key** ; rien ne les porte | une clé bavarde ne peut être ni bridée ni élargie individuellement — le seul levier est le canal | S | ✅ **soldée par M6ter, la décision prise avant le code plutôt qu'à sa place**. Platform-admin seulement (§5.4) — jamais le porteur de la clé, pour qui un plafond auto-réglable n'en serait plus un. `ApiKey::publish_per_minute`, `PATCH /apikeys/{id}/limit`, lu depuis `Grant::ApiKey` au moment de publier plutôt que rechargé ; colonne `BIGINT` pour que le `u32` du domaine ne se voie jamais refuser sa propre valeur, vérifié contre un vrai Postgres |
| D25 | Un upload presigné jamais référencé n'est collecté par personne | 25 MiB par presign, par tout publisher, sans borne et sans trace en base | M | ✅ soldée par M4vicies — `attachment_presigns` enregistre chaque signature, le balayage collecte après un jour de grâce ; migration jouée contre une vraie base (T24) |
| D26 | Le web n'affiche aucune image de notification | — | S | ✅ **soldée par M4quatervicies**, et vérifiée sur le chemin complet — presign, upload de 338 KiB, publish, et la page qui l'affiche. Elle en a sorti une seconde moitié que personne n'attendait : `img-src` ne nommait pas l'origine du bucket, donc un bundle **construit** aurait bloqué l'image que le serveur venait de signer, sans que le développement puisse le voir (la CSP n'est posée qu'au build) |
| D27 | `ReceiptRepository::has_seen` n'a aucun appelant | — | M | ✅ **soldée par M4quinvicies, et elle avait raison d'exiger une décision d'abord**. `has_seen` est supprimé plutôt que câblé : le badge ne pose pas cette question-là mais un agrégat par canal (`NotificationRepository::unread_counts`). La décision de modèle qu'elle réclamait est prise et écrite dans §7.3 — l'opt-in `receipts_enabled` gouverne **qui est mis au courant**, pas si la lecture est enregistrée ; sans ça le badge serait resté égal à la taille du canal sur tout canal par défaut. Le correctif ne fuit pas : `Visibility::Reader` écrit la ligne et n'appende qu'un `Audience::User`. Deux rendus sur trois (web, TUI) ; tray et dock attendent M5 |
| D28 | §13 promet que les changements de trust policy sont journalisés ; le réconciliateur n'audite rien | — | M | ✅ **soldée par M4septvicies, et les trois décisions qu'elle exigeait sont prises**. L'acteur est `{"kind":"configuration"}`, élargi dans le **port d'audit seul** — un cinquième `Principal` serait représentable dans une adhésion et dans une publication. Une entrée **par changement**, ce qui a demandé `managed_declarations` : le réconciliateur applique le fichier entier et ne savait donc jamais ce qui avait bougé. Les **trois** choses réconciliées, sans `managed.service_account.removed` puisque rien ne retire un compte |
| D29 | Les révisions écrites avant M4quatervicies gardent leur ULID | — | S | ✅ **close par constat, et le constat est la seule chose qui la fermait** : aucun déploiement réel n'a jamais tourné, donc les seules lignes concernées sont dans une base de développement que `task dev:reset` efface. La migration de rattrapage — joindre `revisions.author` aux profils et remplir les `display_name` manquants — n'est pas écrite, **et c'est délibéré** : elle n'aurait aucune donnée à corriger et serait une migration jamais jouée dans un dossier où tout le reste l'a été (T24). Si un jour une instance tourne sur un build antérieur à celui-ci, cette entrée est le rappel qu'elle sera à écrire |
| D30 | L'image ne s'affiche que dans la card détail | §10.3 fait de la notification « un composant unique décliné » en quatre surfaces — row d'inbox, card détail, toast, notification OS — et une seule la rend ; dans la liste, une notification avec image est identique à une sans | M | ✅ **soldée par M6bis, la dette la plus ancienne encore ouverte, et vue plutôt que seulement écrite**. §15 a été élargi en connaissance de cause plutôt que contourné : `notify-avatars::ImageAttachmentThumbnailer` partage le pipeline de défense de l'avatar (dimensions avant décodage, formats animés refusés, ré-encodage), `Fanout::derive_thumbnail` le déclenche après chaque publish/revise qui montre un attachment — en dernier, jamais avant la diffusion temps réel — et `GET /attachments/{id}/thumbnail` sert le résultat par l'API, comme l'avatar, pour éviter une signature par ligne d'une liste virtualisée. En la regardant : trois routes qui documentaient un 404 qu'aucune n'a jamais rendu — détail ci-dessous |
| D31 | Les `[[trust_policy]]` ne sont pas échangeables à chaud | — | M | ✅ **soldée par M4undetricies, l'incrément suivant celui qui l'a ouverte**. `notify_app::LivePolicies` tient l'ensemble derrière un `RwLock` partagé ; l'échange de jeton le relit à chaque exchange et la console lit **le même objet** — il était construit deux fois, ce qui aurait donné deux réponses à « quelle forge peut publier » dont une que personne n'applique. `adopt` remplace l'ensemble entier, parce que le retrait est la moitié qu'une fusion perdrait et la seule qui enlève un droit |
| D32 | §7.1bis promet que le serveur expose lui-même un endpoint push UnifiedPush entrant ; aucune route de §8 ne le sert | un distributeur **embarqué** dans l'app Android n'a personne à qui parler côté serveur — seul le chemin « distributeur système » (ntfy…) fonctionne, et encore, une fois qu'un device existe pour le prouver | M | ⬜ ouverte — ouverte par l'incrément qui a écrit le transport sortant (M6) plutôt que de faire semblant que la moitié entrante n'existait pas ; ce que c'est et ce que ça n'est pas, ci-dessous |
| D33 | L'image de dev ne porte pas le SDK/NDK Android | M6 ne peut construire ni le Kotlin (enrôlement Android Keystore, distributeur embarqué, §10bis.4) ni rien qui en dépende — pas de toolchain à essayer, encore moins un `cargo tauri android build` | S | ⬜ ouverte — **sans contournement possible ici**, même raison que D13 : le pod entier est sans root. Le correctif amont est prêt dans `docs/upstream/che-min-mise-android.md` et n'est pas appliqué ; contrairement à D13, l'appliquer ne soldera pas M6 à lui seul — D32 reste ouverte séparément, et le Kotlin reste à écrire |
| D34 | Aucun runner macOS dans l'infra self-hosted de ce projet | le Swift de M7 (Notification Service Extension, enrôlement Keychain, tout le build iOS) ne peut ni être écrit-et-vérifié, ni même compilé, ici — Xcode ne tourne pas sur ce Linux | M | ⬜ ouverte — **une forme différente de D33** : celle-ci a un diff amont prêt (`docs/upstream/che-min-mise-android.md`) ; celle-ci n'en a aucun, parce qu'aucune image ne fait tourner Xcode sur du matériel non-Apple. Le correctif est un runner macOS (GitHub-hosted ou un Mac que le projet possède), pas un changement de code, et reste ouverte jusqu'à ce que cette infra existe — détail ci-dessous |

Les vingt-cinq entrées closes — vingt-trois soldées, une **close par constat** (D29, il n'existe aucune donnée à rattraper) et une **tranchée en risque accepté** (D9) — gardent leur texte ci-dessous plutôt que d'être effacées : une dette dont on ne peut plus relire la raison redevient un souvenir, et c'est exactement ce que cette section refuse.

**D1 — `Event` rendu en type inhabité.** progenitor voyait `allOf[Audience(oneOf), EventPayload(oneOf), {cursor, logged_at}]` et produisait `pub enum EventVariant0 {}` : la désérialisation d'une page de sync ne marchait que tant qu'elle était vide. `notify-client-core` contournait avec son propre modèle minimal (documenté dans le module), donc rien n'était cassé — mais la promesse d'ADR-0009, « les clients viennent du contrat », avait un trou.

*Solution, faite* : **décrire `Event` à la main plutôt que le laisser composer**. Un `impl ToSchema` qui émet un `oneOf` de cinq objets complets — un par `event`, chacun portant déjà `channel_id` ou `user_id`, `cursor` et `logged_at`. **La forme sur le fil n'a pas changé d'un octet** ; seule sa description a changé, et elle est devenue quelque chose qu'un générateur sait modéliser : `Event::Revision { … }` jusqu'à `Event::Preference { … }` dans le client Rust.

Un écart avec la solution écrite, dans le sens qui coûte moins : la charge utile de chaque branche reste un `$ref` vers le schéma dérivé du type, et non une liste de propriétés recopiée. `allOf[$ref, objet]` est la forme que progenitor fusionnait **déjà** correctement — `EventPayload` en était la preuve dans le client généré — donc seule l'enveloppe est écrite à la main, et un champ ajouté à `RevisionEvent` atteint encore le document tout seul. Le coût résiduel est couvert comme prévu : un test sérialise un événement de chaque genre et vérifie les deux sens (rien d'obligatoire ne manque, rien de porté n'est indécrit). Détaillé en §14.1, « Solder D1 et D3 », avec les deux choses que la rigueur du type généré a fait apparaître.

**D2 — pas de mise à jour de canal.** §9.1 promet `notify channel set-ttl` et §10.4 un écran d'admin canaux, alors que §8 n'avait aucune route pour changer une policy. L'incohérence était dans la spec, pas dans le code ; §8 porte désormais la route.

*Solution, faite* : `PATCH /api/v1/channels/{id}`, corps = policy partielle (`default_ttl`, `max_ttl`, `default_priority`, `receipts_enabled`), qui **refuse un canal managé** comme `DELETE` le fait déjà, exige le scope `manage`, et s'inscrit à l'audit avec la policy **résultante** — un log d'audit se relit pour savoir à quoi c'était réglé, et un corps partiel ne répond pas à cette question. La CLI (`channel set-ttl`) et l'écran web en découlent.

Trois décisions prises en l'écrivant :

- **le domaine refuse une policy sous laquelle le canal ne saurait plus publier.** Après application, `TtlPolicy::resolve(None)` est rejoué et son refus est renvoyé : un `default` à `forever` sur un canal qui ne l'autorise pas ferait échouer *chaque* publication à venir, très loin de l'édition qui l'a causé. C'est la vérification faite là où l'erreur est faite ;
- **un `PATCH` qui ne change rien n'écrit rien.** `edit_policy` répond si quelque chose a bougé ; sinon la réponse est la même — un `PATCH` est idempotent — mais sans ligne en base ni entrée d'audit affirmant un changement qui n'a pas eu lieu ;
- **le refus « managed » est un 409**, ce qu'il n'était pas (voir les trouvailles de M2.6–M2.8).

**D3 — items paginés inlinés.** `Page_Channel.items` décrivait l'objet au lieu de référencer `Channel`, donc progenitor frappait `PageChannelItemsItem` — un second type nominal pour la même chose. Le client TS s'en moquait (typage structurel), le Rust non ; `notify-client-core` masquait la verrue derrière un alias pour qu'elle n'atteigne pas les frontales.

*Solution, faite* : un `impl ToSchema` manuel pour `Page<T>` où `items` est un tableau de **référence** vers `T`, au lieu de laisser utoipa inliner le paramètre générique. Deux détails qui ne se devinent pas : un corps générique n'est pas décrit mais **composé** (`ComposeSchema` ; `PartialSchema` en découle par l'implémentation générale d'utoipa), et le `name()` d'un type générique est un **préfixe** auquel utoipa ajoute celui de l'élément — le nommer en entier donne `Page_Channel_Channel`. Effet de bord agréable, obtenu : le document a rétréci d'une copie complète de `Channel`, `Notification` et `Revision`, et les alias `ChannelSummary`/`InboxEntry` ont disparu de `notify-client-core`.

**D4 — le mode `:` parse mais n'exécute pas.** La ligne passe par le parseur clap du shell (c'est ce qui garantit la parité §9) et le résultat s'affiche, mais la commande n'est pas lancée ; les verbes fréquents ont leurs raccourcis directs.

*Solution, faite* : l'exécution est sortie de `main`. Chaque sous-commande **renvoie une `String` rendue**, `main` l'imprime, et le TUI appelle la *même* fonction de dispatch avec la session de sa fenêtre puis met le retour dans son popup. Ce n'était pas un refactor de confort : c'est la règle §9.3 (« toute logique qui apparaît dans `notify-cli` doit redescendre ») appliquée à la présentation.

Deux détails qui font la parité plutôt que l'illusion : le `--output` **de la ligne** est honoré, donc `:ls --output json` veut dire au prompt `:` ce qu'il veut dire au shell ; et le `match` du dispatch est exhaustif, donc une sous-commande ajoutée à `Command` ne compile plus tant que les deux appelants ne la connaissent pas. `:watch` est refusé explicitement — depuis le TUI, ça ne peut vouloir dire qu'ouvrir la fenêtre dans laquelle on est déjà.

**D5 — pas de signal local.** §9.3bis veut qu'une socket unix réveille les frontales lectrices dès que la détentrice du flux écrit dans le store. Sans elle, une lectrice ne voit rien tant qu'on ne rafraîchit pas — latence, pas incorrection, et le TUI affiche en permanence dans quel mode il est.

*Solution, faite* : la détentrice écoute sur `store.stream.sock` et envoie un octet après chaque écriture ; les lectrices rechargent depuis le store à la réception, tick existant en filet. Détaillé en M2.4 ci-dessus, y compris les trois propriétés qui la rendent non porteuse et le fait que Windows n'est pas câblé.

**D6 — `delivered_at` non conservé.** §7.2 veut le badge « reçue en retard (+2h13) » calculé côté client à partir des deux horodatages — le serveur les fournit tous les deux ; c'est le client qui ne garde pas le sien.

*Solution, faite* : une colonne `delivered_at` dans le store local, **écrite par le store lui-même** — c'est un fait sur ce cache-ci, pas quelque chose qu'un appelant devrait pouvoir passer de travers — à la première insertion et jamais réécrite (la clause `ON CONFLICT` nomme ses colonnes une à une pour ça). Schéma en version 2 par `ALTER TABLE`, donc un cache existant n'est pas jeté ; les lignes qu'il contenait déjà n'ont **pas** d'heure d'arrivée, et n'en inventent pas — un horodatage deviné mettrait un badge sur toutes.

Le retard est `delivered_at - created_at`, au-delà d'un seuil de cinq minutes en dessous duquel on mesure de la dérive d'horloge, et il se lit `+2h13` comme §7.2 l'écrit. Il s'affiche des deux côtés : sur une ligne venue du serveur (le cache répond l'heure d'arrivée, le serveur l'heure de création — aucun des deux n'a les deux) comme sur une ligne venue du cache. Le web garde sa version limitée aux arrivées du flux tant qu'il n'a pas de store, ce qui est déjà écrit dans sa vue.

**D7 — les deux legs externes de la matrice OIDC.** Détaillé en §15 : la suite est en place et paramétrée, elle échoue bruyamment si on nomme un fournisseur injoignable, et seul le leg `mock` tourne.

*Solution* : démarrer le fournisseur **dans une étape** et non en `services:` (ceux-là démarrent avant le checkout, donc aucune config rendue ne peut y être montée), puis exporter `WNK_TEST_OIDC_ISSUER` et les identifiants. Pour Dex, une config `staticClients` + `enablePasswordDB` avec alice et **aucun mapping de groupes** — cette absence *est* le test (T29). Pour Authentik, le blueprint déjà commité dans `dev/authentik/`.

*Où ça en est, précisément.* Tout ce qui est fabricable ici l'est, et **rien de ce qui demande un fournisseur vivant n'a été observé** — cet environnement n'a aucun moteur de conteneurs (§12.4, le voisin de D13), donc ni Dex ni Authentik n'ont pu être démarrés pour regarder.

Fait et vérifié : la **config Dex** que §15 signalait comme absente du dépôt existe (`dev/dex/`, un template plus `render.sh` qui génère mot de passe et secret client à chaque exécution et les hashe en bcrypt — vérifié en le lançant, hash `$2a$` valide, fichiers rendus gitignorés) ; un service `dex` dans le compose et un `task dev:dex` pour reproduire à la main un leg rouge en CI ; et la suite paramétrée là où elle devait l'être — `WNK_TEST_OIDC_USERNAME`, parce que **le connecteur local de Dex authentifie sur l'`email`** et non sur le `username`, donc « alice » n'y est pas un login. C'est exactement le genre d'écart que la matrice existe pour faire apparaître ; le coder en dur aurait été en cacher un.

Écrit et **jamais exécuté** : les deux étapes de démarrage dans `.forgejo/workflows/oidc-compat.yaml`, et la matrice qui passe de un fournisseur à trois. Ce qui rend ça tenable plutôt qu'imprudent est une propriété de la suite et non du workflow — **nommer un fournisseur injoignable panique au lieu de sauter**, donc un leg mal câblé devient rouge en nommant la variable qui manque. Aucune de ces étapes ne peut produire la panne que ce job existe pour empêcher : un tic vert qui veut dire « rien n'a tourné ». La première exécution sur une vraie forge est le test, comme pour D12.

Deux inconnues nommées plutôt que supposées résolues : que le grant `password` d'Authentik réponde pour un client **public** (le blueprint le déclare ainsi), et que la socket Docker montée dans le job suffise à joindre depuis le conteneur de job ce qu'il a démarré (d'où `--network host`). Les deux échouent bruyamment.

**D8 — un contrôle vert qui ne prouvait rien.** En écrivant M2, `task recu` échouait (D-avatar, corrigé) et le contrôle d'idempotence — « les fichiers générés n'ont pas bougé » — **passait justement parce que la génération n'avait pas tourné**. C'est le même mode d'échec que D7 sous une autre forme, et il a été commis ici après avoir été dénoncé deux sections plus haut.

*Solution* : la règle, écrite pour qu'elle serve ailleurs — **une vérification doit prouver que le travail a eu lieu avant de vérifier son résultat**. Concrètement : contrôler le code de sortie *et* une trace d'exécution (`task recu` nomme ce qu'il régénère) avant de comparer des empreintes. Le job `contract` en CI était protégé par son code de sortie ; c'est la vérification manuelle qui ne l'était pas.

**D10 — une mesure locale qui contredisait son propre gate.** `task test:cover:domain` disait mesurer « le périmètre que le gate CI applique réellement » et lisait la couverture par **région** d'un run `-p notify-domain -p notify-app`, là où le gate lit des **lignes** sur un run **workspace**. Deux écarts, tous deux vers le bas : 90,3 % affiché contre 96,2 % appliqué. Le coût n'est pas théorique — M3.5 a enregistré une baisse de couverture qui n'existait pas, et la fermeture de M3 a bien failli ouvrir une dette pour la réparer.

*Solution, faite* : la tâche **exécute le gate** (`xtask coverage-gate` sur le LCOV du run workspace) plutôt que d'approximer ce qu'il ferait. Le raisonnement vit dans `.tasks/test.yaml`, au-dessus des commandes, pour que le prochain à vouloir la rendre plus rapide voie d'abord pourquoi elle ne l'est pas.

La règle générale, à côté de celle de D8 : **une vérification locale qui nomme un gate doit lancer ce gate.** Une approximation qui se trompe vers le haut cache un échec ; une qui se trompe vers le bas fabrique du travail — et coûte la confiance qu'on accorde au chiffre la fois d'après.

**D11 — deux drapeaux que §9.1 promet et que la CLI n'avait pas.** Le coût s'est vu en écrivant M4.7 : une notification de CI sans bouton « voir le run » est précisément celle qui en voudrait un, et §2.3 fait de `view` la première des cinq actions.

*Solution, faite* : `--action "type:label:argument"` et `--image ./graph.png`. Trois décisions valent d'être relues.

**Le découpage s'arrête à trois morceaux**, ce qui laisse l'argument contenir des `:` — et il en contient toujours, puisque le cas courant est une URL. Ce que cette forme ne sait pas dire est un *label* contenant un `:`, et c'est le bon abandon : une légende de bouton est courte et se renomme, une URL ne se réécrit pas pour éviter son schéma.

**`choice` et `silence` sont refusés plutôt que tordus.** L'un porte une liste d'options, l'autre une portée et une durée ; les aplatir dans un champ séparé par des `:` aurait inventé une syntaxe que ce document n'a pas. Le refus dit où la forme rentre : l'API.

**Aucun `--action` laisse les boutons tranquilles.** §2.3 remplace les actions **intégralement** à chaque révision, donc envoyer une liste vide *effacerait* les boutons d'une notification révisée pour une raison sans rapport — exactement ce qu'un `notify update --status success` fait tout le temps en CI. Le champ part donc absent et pas vide, et un test le verrouille parce que la différence est invisible à la lecture.

Et `--image` a trouvé autre chose, qui est D14.

**D14 — `POST /attachments/presign` n'est servi par aucun build.** §8 le liste, §6.1 en décrit le contrat (TTL court, `content-length-range` et content-type imposés, upload direct au bucket), `AttachmentStore::presign_upload` existe comme port et `S3Attachments` l'implémente depuis M2.2 — **et aucune route ne l'expose**. Découvert en câblant `--image` : au-dessus de 48 KiB, §2.3 dit « passer par un attachment », et il n'y a pas de chemin.

La conséquence n'est pas propre à la CLI : *personne* ne peut publier une image de plus de 48 KiB, quel que soit le client, parce que le seul moyen d'en déposer une n'existe pas. C'est la même famille que le port `PushDispatcher` sans registre de M1 — un port de production complet dont rien n'atteint la porte.

*Solution, faite* : `POST /api/v1/attachments/presign`. Quatre choses valent d'être relues.

**Le corps nomme un canal, et c'est l'autorisation.** Uploader est l'acte d'un publisher, donc la route exige `Scope::Publish` sur le canal visé. L'alternative — « tout principal authentifié » — donnerait à un membre en lecture seule, c'est-à-dire à peu près à tout le monde, de quoi écrire des octets arbitraires dans notre object store : une primitive de remplissage de disque offerte au public le plus large de l'API, en échange d'un champ de moins sur un corps de requête. L'attachment n'est pas *lié* au canal pour autant : le lier voudrait dire décider ce qu'il devient quand le canal est archivé, ce que §7.2bis ne demande pas.

**Tout est vérifié avant de signer**, parce que §6.1 fait de la signature *l'*accès : après elle, le client parle au bucket et plus à nous, et il n'y a pas de second contrôle. Les 25 MiB de §8.2 sont donc refusés avant qu'une URL existe — une URL signée pour un objet trop gros est une URL émise contre notre propre règle, et qu'on ne peut pas reprendre.

**Le type est normalisé puis épinglé.** `IMAGE/PNG; charset=binary` est accepté — refuser un client pour de la ponctuation est un ticket de support, pas une défense — et c'est la forme *normalisée* qui part dans la signature, sans quoi `image/png; x=<n'importe quoi>` serait validé comme une chose et imposé comme une autre.

**Et la CLI traverse le plafond toute seule** : `--image ./graph.png` inline en dessous de 48 KiB et passe par le bucket au-dessus, sans que personne ait à savoir de quel côté sa capture d'écran tombe. Exposer une limite d'implémentation sous forme de drapeau aurait été la faire porter à l'utilisateur.

**Une trouvaille en le branchant, et c'est encore D1/D3** : `PresignedUpload.headers` était un `Vec<(String, String)>`. Un tuple Rust est un **tableau de longueur fixe** en JSON Schema — `prefixItems` plus `items: false` en 2020-12 — et un `items` booléen n'est pas exprimable en OpenAPI 3.0, donc la génération du client Rust s'est arrêtée net à la seconde où ce type a atteint le document. C'est le même enseignement pour la troisième fois : une forme qu'aucun générateur ne sait modéliser n'est pas descriptible, si naturelle soit-elle à lire en Rust (ADR-0009). C'est devenu une map, ce qui est de toute façon la bonne forme — des en-têtes sont nommés, pas ordonnés.

**Vérifié en le lançant**, contre le vrai RustFS de l'env de dev : presign → `PUT` signé accepté par le bucket (200) → publish référençant l'`att_…`. Et les refus : 413 au-dessus de 25 MiB, 422 pour un type non servi et pour une longueur nulle, 403 pour qui ne peut pas publier là. Côté CLI, une image de 4 KiB part en `data_uri` et une de 200 KiB en `attachment`.

**D15 — une jauge sans exporteur.** §13 nomme `sink_dlq_depth{channel}` parmi les métriques Prometheus, et M4bis a livré tout ce qu'il faut pour la remplir : `SinkQueue::dead_letter_depth` répond la profondeur par canal, testé contre un vrai Postgres. Ce qui manque n'appartient pas aux sinks : **ce build n'expose aucun `/metrics`**, donc §13 entier attend son incrément. Câbler une jauge derrière un exporteur qui n'existe pas serait un compteur que personne ne lit — et l'écrire quand même donnerait l'impression que l'observabilité est faite.

*Solution, faite* : **M4ter**, qui monte l'exporteur et branche les six familles d'un coup — port `Metrics`, adapter `prometheus-client`, `GET /metrics` à côté des sondes. `sink_dlq_depth` est rafraîchie par la boucle de drain après une passe qu'elle a menée, et par personne d'autre.

Un écart avec la solution écrite, et il va dans le sens de la prudence : la jauge n'est pas rapportée à zéro par les suiveurs, elle est **absente** chez eux. Le texte ci-dessus disait « câbler la jauge », ce qui laissait entendre une valeur par replica ; l'écrire ainsi aurait fait dire à trois replicas sur quatre « rien n'est coincé » pendant qu'un webhook s'entassait chez le quatrième. L'alerte se formule donc sur le `max` entre replicas, ce que la page `ops/observability` écrit noir sur blanc avec sa requête.

**D18 — une route de §8 que rien ne servait, la seconde.** Trouvée en diffant la liste d'endpoints de §8 contre les chemins d'`openapi.json` : 45 promis, 49 servis, et **une** ligne d'écart une fois les noms de paramètres normalisés — `GET /api/v1/channels/{id}`. Même famille que D14, même méthode, et la même question à se poser : ce diff prend dix secondes et n'était une porte de personne.

*Solution, faite* : **M4sexies**, la route et son use case, avec le refus qui compte — un canal invisible et un canal inexistant rendent la même réponse, testé sur cette porte plutôt qu'hérité en confiance.

Il reste un écart, plus petit et nommé : §8 décrit le détail comme « membres, TTL, sinks, badge managed », et **les sinks n'y sont pas**. Ils ne vivent pas sur `Channel` mais dans `[[channels.managed.sinks]]`, et les servir ici publierait des destinations — une URL de webhook, une adresse mail — à tout membre du canal, quand ADR-0018 tient à ce que la file ne porte jamais leur secret. Le champ mérite sa propre décision (kinds seulement ? rôle §5.4 seulement ?) plutôt qu'une forme choisie en passant.

**D20 — le mode `:` parse, ne complète pas et n'a pas de mémoire.** §9.2 promet « complétion contextuelle depuis l'arbre clap + historique persisté » ; D4 a livré l'exécution (la ligne est découpée puis passée à `Cli::try_parse_from`), et ni l'une ni l'autre des deux moitiés restantes. Le coût n'est pas fonctionnel — tout reste atteignable — mais il porte sur ce que le mode `:` était censé être : le filet de découvrabilité qui rend le reste accessible sans quitter le TUI. Sans complétion, il faut déjà savoir ce qu'on cherche.

*Solution, faite* : **M4undecies**, et ce qui les rendait faisables ici est ce que la dette annonçait — les deux moitiés sont **testables sans terminal**. La complétion se dérive de `Cli::command()`, donc une fonction pure sur l'arbre clap ; l'historique est un fichier du répertoire de profil (§9.3bis), en `0600`. Restent les touches, qui sont du câblage et se testent en vérifiant qu'elles appartiennent à la ligne tant qu'elle est ouverte et rendent la main dès qu'elle se ferme.

**D9 — le refresh token est lisible par un XSS, et le plan de M3 ne marche pas.** Détaillé en fin de M3.6 : ADR-0016 disait « M3 closes this », le service worker existe, et le déplacement tel qu'imaginé n'achète rien — un worker est tué agressivement, donc pour survivre le jeton devrait vivre en IndexedDB, que la page lit aussi bien que lui. Le garder en mémoire du worker protège vraiment, et coûte un aller-retour complet chez l'IdP à chaque éviction : c'est « tout en mémoire », l'option qu'ADR-0016 avait écartée en M1.

*Tranchée* : **ADR-0020, option A** — le jeton **reste** en `sessionStorage`, et l'ADR devient le document qui empêche de l'oublier. Il passe `accepted` et **supersède ADR-0016**, dont la phrase « M3 closes this » est précisément ce qui a cessé d'être vrai.

Le raisonnement qui a fait pencher de ce côté est celui que l'option C ne pouvait pas rendre bon marché : son prix est une phrase de §5.1 — « zéro session serveur » — plus une clé symétrique à monter et faire tourner, ou une table à répliquer. C'est de l'architecture réelle achetée contre un risque résiduel dont les deux conditions d'exploitation se sont rétrécies depuis ADR-0016 : un script injecté doit désormais passer une CSP sans `unsafe-inline` dans `script-src` et ne peut joindre que l'API et le fournisseur via `connect-src` (ADR-0019), et le seul endroit qui rend du contenu de publisher passe trois portes avant d'être du HTML. Ce que le jeton rapporte à un attaquant n'a pas changé — des heures au lieu de quinze minutes — mais ce qu'il faut faire pour l'atteindre, oui, et le prix de C, non.

**Ce sur quoi la décision repose**, écrit parce qu'une décision appuyée sur des choses que personne n'a notées est une décision qui cesse de tenir en silence : la CSP livrée avec le bundle (ADR-0019) ; le rendu markdown qui n'interprète jamais de HTML brut ; la rotation des refresh tokens, qui rend un jeton volé à usage unique et sa réutilisation détectable chez le fournisseur ; et la portée onglet de `sessionStorage`.

**Ce qui rouvre le dossier** : n'importe laquelle de ces quatre qui faiblit — un `unsafe-inline` ajouté pour faire marcher une bibliothèque en est le chemin le plus probable —, ou un déploiement dont les canaux portent quelque chose dont la divulgation est pire que l'accès API qu'un XSS a déjà. Le contrôle porteur est maintenant la CSP et non l'adresse du jeton : une régression de CSP est une régression de jeton, ce qui vaut d'être su avant de la relâcher.

**D19 — le TUI ne relaie rien au bureau.** §9.2 promet « option `--notify-send` qui relaie vers les notifications desktop (notify-rust) quand le terminal n'a pas le focus », et §7.4 s'appuie dessus en écrivant que « le TUI `--notify-send` » supprime ses popups pendant un DND. Le drapeau n'existe pas, `notify-rust` n'est pas une dépendance du workspace, et la règle de §7.4 s'applique donc à une option absente — ce qui est la forme la plus discrète d'une promesse fausse : deux sections qui se citent l'une l'autre à propos de rien.

*Solution, faite à moitié* : **M4quaterdecies**. `notify-rust` derrière le drapeau, et la question qui méritait d'être tranchée avant d'écrire l'a été dans le sens prévu — **la clause « quand le terminal n'a pas le focus » est retirée de §9.2**, parce qu'aucune façon portable de le savoir n'existe et qu'un heuristique par plateforme serait faux chez quelqu'un sans qu'on puisse le déboguer d'ici. Le drapeau relaie quand il est allumé.

« À moitié » est littéral, et c'est la raison du jaune : la **règle** de §7.4 est portée par une fonction pure avec neuf tests — fenêtre dans son propre fuseau, exception qui sonne quand même, portée par canal, `urgent` qui ne passe que si le destinataire l'a dit, et la queue de nuit qui appartient à la veille. L'**envoi**, lui, n'a jamais tourné : il n'y a ni session desktop ni bus D-Bus dans ce workspace. La ligne qui appelle le démon est écrite et n'a été exécutée par personne.

**D22 — la même règle, écrite trois fois.** Ouverte par l'incrément qui a ajouté la troisième copie, ce qui est la seule façon honnête de l'ouvrir. « Est-ce que ça doit faire du bruit maintenant ? » est décidé par `notify_domain::Dnd::is_muted` côté serveur (pour le push), par `web/src/stores/preferences.ts` côté navigateur (pour les toasts), et depuis M4quaterdecies par `notify-cli/src/tui/desktop.rs` (pour le relais desktop). Quatre clauses, un ordre qui compte, un fenêtrage qui traverse minuit et dont **la queue appartient à la veille** — c'est-à-dire exactement le genre de règle qui diverge sans que personne s'en aperçoive, et ce projet en a déjà attrapé deux (le `PUSH_PAYLOAD_HINT` de M3.5, le `TestLeaderElection` de M4bis).

Pourquoi trois : le client ne peut pas atteindre `notify-domain`. Toute la pile cliente est bâtie sur le contrat généré (ADR-0009), et c'est une décision, pas un accident — un client qui dépend du domaine serait un client qui suit une refonte interne. Le navigateur, lui, n'atteint pas Rust du tout.

*Solution, faite* : **(a)**, choisie parce qu'elle s'est révélée moins chère que son écriture ne le laissait croire — le serveur ne calculait pas seulement la réponse, il la **jetait**. `SyncBacklog` remplit `deferred_by_dnd` par appelant et le drain du flux gardait les événements sans la liste. `muted_by_dnd` sur l'enveloppe transporte cette réponse, posée par les deux transports au moment où le lecteur est connu, jamais stockée.

Ce que craignait la question ouverte — « que veut dire *maintenant* pour un événement lu trois minutes plus tard » — se répond tout seul dans cette forme : le drapeau est calculé **à la livraison**, pas à l'écriture, donc il dit ce qui était vrai quand la trame est partie vers ce lecteur-là. Un client qui rattrape une semaine de backlog reçoit les drapeaux tels qu'ils étaient au moment de son rattrapage, ce qui est exactement ce qu'une décision de popup demande : faut-il faire du bruit **en te le montrant maintenant**.

La copie TypeScript du web est partie avec (M4sedecies) : c'était une modification de store, avec ses tests, sans surface visuelle — donc pas la question que D21 pose. **Il ne reste qu'une implémentation de la règle**, celle du domaine. La moitié `silence` (§2.3) reste décidée côté client et ce n'est pas la même dette : c'est une donnée que le client détient déjà, lue localement, pas une règle réécrite.

**D28 — le réconciliateur n'écrit rien dans le journal d'audit.** §13 liste les changements de trust policy parmi les actions journalisées, et le vocabulaire portait `trust_policy.changed` — sans émetteur. M4duovicies l'a **retirée** plutôt que gardée : une valeur que rien ne produit est la moitié inoffensive du défaut qui a cassé `GET /admin/audit`, et la garder aurait laissé la liste mentir dans l'autre sens.

Ce que ça coûte : `[[trust_policy]]` est ce qui autorise une forge à publier dans un canal (§5.3). L'éditer élargit ou restreint qui peut écrire sur cette instance, et cette modification ne laisse **aucune trace** dans le journal qu'on rejoue après un incident. Le fichier est en Git, donc l'historique existe ailleurs — c'est ce qui rend la dette moyenne plutôt que grave — mais « demander à Git » et « lire le journal d'audit » ne sont pas la même opération à 3 h du matin, et §13 promet la seconde.

*Solution* — la mécanique est petite, la décision ne l'est pas. Le réconciliateur sait déjà ce qu'il applique ; ce qu'il n'a pas, c'est **ce qui a changé** (il applique le fichier en entier, sans le comparer à l'état précédent) et **un acteur**. Personne n'a « fait » l'action : c'est un fichier, appliqué par un processus, au démarrage ou sur `SIGHUP`. Trois questions à trancher avant d'écrire :

- **quel acteur** — un `Principal` système n'existe pas dans §5, qui en compte quatre et les fait tous humains ou machines *authentifiées*. En inventer un cinquième pour l'audit touche le modèle d'identité ;
- **quelle granularité** — une entrée par pass de réconciliation, ou une par policy ajoutée, retirée ou modifiée ? La seconde demande le diff que le réconciliateur ne calcule pas ;
- **et jusqu'où** — la même question vaut pour les canaux managés et les service accounts, qui sont réconciliés par le même code et tout aussi peu journalisés. Traiter les trust policies seules réglerait un tiers d'un problème en donnant l'impression du tout.

**D26 — une image publiable que personne ne voit.** M4unvicies a donné aux pièces jointes leur chemin de lecture ; il reste que **le web n'affiche aucune image de notification**, attachment ou `data:`. `rtk grep -rn attachment web/src`, code généré exclu, rend zéro résultat. Le contenu est donc publiable (§2.3), servi (§8), présent dans les deux clients générés, et invisible.

Ce que ça coûte : un publisher qui joint une capture à un incident la voit disparaître entre le `curl` et l'écran. C'est une promesse fausse de §2.3 plutôt qu'une capacité manquante — tout est là sauf le `<img>`.

*Solution* : rendre `content.image` dans la vue détail et dans la ligne d'inbox, les deux formes (`data_uri` directement, `attachment` via `GET /attachments/{id}` puis l'URL signée), avec les précautions de §10.3 — dimensions bornées, pas de décalage de mise en page pendant le chargement, et une image cassée qui ne casse pas la ligne. **C'est du rendu**, donc exactement la question que D21 pose : la logique se teste sous jsdom, l'apparence n'est relue par personne dans cet environnement. Une CSP à vérifier au passage : ADR-0019 doit autoriser `img-src` vers l'origine du bucket, ce qui n'est pas l'origine de l'API.

**D27 — un compteur de non-lus que rien ne compte.** `ReceiptRepository::has_seen` porte ce commentaire : « la seule question que pose le badge non-lus, et pas la peine de charger tous les receipts pour y répondre ». Il est implémenté par `PgStore` et par le double, et **aucun appelant**. Le badge, lui, est promis trois fois : compteur non-lus par canal dans la sidebar (§9.2 pour le TUI, §10.4 pour le web), ligne d'état du tray et badge dock/taskbar (§10bis.3bis, §10bis.3ter), avec la précision que les trois « proviennent du même état de `notify-client-core` — trois rendus, une seule vérité ». Aucun des trois n'existe.

Ce que ça coûte est modeste et vaut d'être dit sans le gonfler : rien n'est cassé, l'inbox se lit, et « non lu » est une commodité. Mais c'est la commodité qui décide si on ouvre l'app, et trois sections la décrivent comme acquise.

*Solution* — et la raison pour laquelle elle n'a pas été enchaînée ici : elle demande une **décision de modèle** avant du code. « Non lu » n'est pas défini par §7.3, qui parle de receipts `seen` **opt-in par canal** : sur un canal sans receipts, personne ne pose de `seen`, donc `has_seen` répond toujours faux et tout resterait éternellement non lu. Il y a donc deux notions à démêler — le receipt `seen` (partagé, visible des autres, opt-in) et le « lu » **local** d'un lecteur (privé, toujours disponible, celui dont un badge a besoin) — et §7.3 n'en décrit qu'une. Choisir en écrivant le code produirait un badge dont personne ne saurait dire ce qu'il compte.

**D25 — l'objet que la base n'a jamais vu.** *Soldée par M4vicies* ; le texte reste, parce qu'une dette dont on ne peut plus relire la raison redevient un souvenir. M4novodecies collectait les pièces jointes dont la notification a expiré. Restaient celles qui n'ont **jamais** été référencées : `POST /attachments/presign` signe une URL et rend un identifiant, le client téléverse, et si rien ne publie ensuite, aucune ligne de la base ne porte cet identifiant. Aucune requête ne peut donc le nommer, et l'objet est orphelin dès sa naissance.

C'est le trou qu'un abus viserait, et il vaut d'être décrit dans ces termes : presigner, téléverser 25 MiB, ne jamais publier, recommencer. L'autorisation de M4/D14 (`Scope::Publish` sur le canal visé) borne *qui* peut le faire à des publishers plutôt qu'à tout membre — c'est ce qui empêche que ce soit ouvert à toute l'instance —, mais elle ne borne pas *combien*, et il n'y a pas de quota d'upload en §8.2.

*Solution, faite* : un **registre des presigns** — `attachment_presigns`, écrite au moment de la signature, `(id, canal, demandé_à)`. La collecte est « toute ligne du registre qu'aucune révision vivante ne référence et qui a passé un délai de grâce ». Le délai n'est pas décoratif : sans lui, un upload en vol entre la signature et la publication serait supprimé par la passe qui tombe au mauvais moment ; il vaut un jour contre une URL présignée qui vit quinze minutes.

Les trois poids annoncés se sont tous vérifiés, et un quatrième est apparu : la **migration** est là (contrairement à la policy de canal, il n'y avait pas de JSON où se glisser) avec un **backfill** qui rattrape les identifiants déjà référencés — mais pas, par construction, les uploads jamais référencés d'avant la migration, ce que le fichier dit à voix haute ; l'écriture supplémentaire sur le chemin du presign est un `INSERT ... ON CONFLICT DO NOTHING` posé **après** la signature ; une ligne veut bien dire « on a signé une URL » et pas « un objet existe », sans danger côté S3 où supprimer une clé inexistante réussit. Le quatrième : `PurgeExpired::new` a franchi les sept arguments, ce qui a produit `Attachments { store, registry }` — un regroupement qui dit quelque chose plutôt qu'un `allow` qui tait un signal.

Le raccourci pesé avant d'écrire la table, et écarté : une **règle de cycle de vie sur le bucket** supprimant tout objet non modifié depuis N jours ne coûte aucun code — mais elle ne sait pas distinguer un orphelin d'une pièce jointe d'incident encore vivante, donc elle ne va qu'avec une rétention plafonnée, ce que §7.2bis n'impose pas. Elle reste la bonne réponse pour qui accepte ce plafond, et c'est une décision d'exploitation plutôt qu'une décision de produit.

**D24 — la moitié de phrase qui reste.** §8.2 ouvre sur « défauts globaux, surchargeables par canal (§11) **et par API key** ». La première moitié est soldée par M4octodecies ; la seconde n'existe nulle part — pas de champ sur `ApiKey`, pas de surface pour le déclarer, pas de lecture.

Ce que ça coûte aujourd'hui est modeste et vaut d'être dit pour ne pas surestimer l'entrée : une clé qui publie trop est déjà bornée par les 60/min de son propre budget et par les 600/min de son canal, et un opérateur qui veut ouvrir le robinet pour un pipeline précis peut le faire **au canal**. Ce qui manque est la granularité — brider *une* clé sans brider le canal, ou en élargir une sans élargir tout le monde.

*Solution* — et la raison pour laquelle elle n'a pas été écrite avec l'autre moitié : la question bloquante n'est pas mécanique mais de droits, et **ni §8.2 ni §5.2 ne la tranchent**. Qui fixe le plafond d'une clé ? Si c'est son créateur, une clé arrive avec le plafond que son porteur a choisi et cesse d'être un plafond. Trois réponses tenables, à départager explicitement :

- **`platform-admin` uniquement** (§5.4) — cohérent avec « les opérations d'instance passent par ce rôle », au prix d'un aller-retour humain pour chaque exception ;
- **le `manage` du canal**, pour les clés scopées à ce canal seulement — le plafond redevient une décision de canal, ce qui rejoint la moitié déjà livrée et pourrait la rendre suffisante ;
- **GitOps**, comme les canaux managés — mais les clés naissent par l'API (`POST /apikeys`), donc ce serait un genre de clé de plus.

Le travail mécanique derrière (un champ, une colonne — les clés sont persistées en colonnes et non en JSON, donc **une migration ici**, contrairement à la policy de canal — et le passage du nombre au limiteur, qui existe déjà depuis M4octodecies) est petit. C'est la décision qui coûte, et une décision de droits prise en écrivant le code est une décision que personne ne relit.

**D23 — un plafond par process pour une limite écrite par instance.** Ouverte par l'incrément qui a posé le plafond (M4septdecies), ce qui est la seule façon honnête de l'ouvrir — comme D22 l'avait été par celui qui avait ajouté la troisième copie. §8.2 publie « connexions SSE/WS par user : 20 » et se lit naturellement à l'échelle de l'instance ; ce qui est appliqué est « 20 par principal **et par replica** », parce que le compte est une `HashMap` dans le process.

Ce qui est **garanti** malgré tout, et qui n'est pas rien : aucun process ne tient plus de 20 connexions pour un même principal. C'est la ressource que le plafond défend — une connexion tenue coûte la mémoire, la tâche et le descripteur de fichier du replica qui la tient, pas de l'instance en général —, donc la propriété de protection est entière sur chaque replica. Ce qui se perd est la lecture littérale du nombre : à trois replicas, un client obstiné en tient 60.

*Solution*, dans l'ordre de coût croissant, et aucune des trois n'est évidemment gagnante :

- **amender §8.2** pour qu'elle dise ce qui est appliqué (« par replica ») — coût nul, et c'est peut-être la bonne réponse : le nombre existe pour borner une ressource par process, et une limite qui décrit ce qu'elle fait vaut mieux qu'une limite qui décrit ce qu'on aurait voulu ;
- **un compteur partagé** (Redis, ou une table Postgres avec bail et heartbeat) — rend le nombre littéral, au prix d'un aller-retour **sur le chemin de connexion** et d'un mode d'échec neuf : un replica tué laisse des baux qui enferment un user dehors jusqu'à leur expiration, c'est-à-dire exactement la panne que le `Drop` du garde local rend impossible aujourd'hui ;
- **plafonner à l'ingress** plutôt que dans l'application, ce qui est où vivent déjà les limites de connexion dans la plupart des déploiements, et sortirait le sujet du code.

À trancher quand un déploiement réel aura plus d'un replica et un usager qui pousse — pas avant : les trois options se départagent sur des faits d'exploitation que personne n'a encore.

**D21 — une capacité serveur que personne ne peut atteindre.** Trouvée en diffant §10 contre `web/src`, le dernier gros document dont les promesses n'avaient pas été comparées au code. `GET /api/v1/search` est servi, documenté, généré dans les deux clients, et **testé dans la suite bloquante** (T27 — le scope par canal, la notification expirée qui disparaît des résultats). Il n'est appelé de nulle part : pas de barre de recherche dans l'inbox web (§10.4 en promet une), pas de commande CLI, et le `/` du TUI filtre localement ce qui est déjà chargé (§9.2) plutôt que d'interroger le serveur.

La cause tient en une ligne de §10.4 : le consommateur prévu est la **palette de commandes** (`Cmd+K`), « une seule surface pour chercher et switcher partout dans l'app », et elle n'existe pas — ni composant `Command` dans le kit vendoré, ni raccourci, ni rien. Le reste de §10.4 est là (shell trois zones, sidebar avec filtres, panneau détail, permaliens `/n/<id>` et `/c/<id>`, écrans Inbox/Admin/Console/Settings) ; il manque la palette, la recherche, et l'écran **Apps** — ce dernier attend qu'il y ait des releases à télécharger, donc D17.

Ce que ça coûte, exactement : le seul chemin vers une notification qu'on ne voit pas à l'écran est de la retrouver à la main dans l'inbox, canal par canal. Rien n'est cassé, et c'est justement pourquoi ça pouvait durer — un endpoint sans appelant ne fait échouer aucun test.

*Solution* : la palette, telle que §10.4 la décrit — `Command` dans un `Dialog`, fuzzy sur les canaux et les écrans, recherche via l'endpoint, actions contextuelles avec le **même vocabulaire que le mode `:` du TUI**. La partie logique (le filtrage, le classement des résultats, le raccourci) se teste sous jsdom comme le reste du front ; **l'apparence, non** — il n'y avait pas de navigateur dans cet environnement, et §10.1/§10.6 font du rendu une affaire de design system. C'est la raison pour laquelle cette entrée a été écrite plutôt que soldée sur place : c'était le premier travail de ce projet dont la moitié qui compte ne serait vérifiée par personne.

*Et c'est exactement ce qui s'est passé*, ce qui vaut d'être gardé écrit : la partie testée était juste, et les trois défauts que M4quatervicies a corrigés en ouvrant la page — deux gris indiscernables, une sélection qui sort de l'écran, une listbox sans sémantique — étaient tous dans la moitié que personne ne regardait. La leçon n'est pas « il fallait tester l'apparence » ; c'est que « écrit et testé » et « vu » sont deux états différents, et que la table de §14.1 a raison de les distinguer.

**D12 — un workflow commité qui n'a jamais tourné.** `.forgejo/workflows/notify.yaml` est écrit contre le contrat de §5.3 et les deux commandes qu'il lance sont vérifiées contre un vrai serveur, mais aucune forge n'existe ici pour répondre des deux dernières lignes : que `id-token: write` rende un jeton, et que son `aud` nomme weebo-notify.

*Solution* : le pousser sur la forge principale et regarder. C'est la seule façon, et le risque est borné pour une raison qui vaut d'être dite — **les deux inconnues échouent bruyamment** : pas de jeton donne une variable vide et une étape rouge, un mauvais `aud` donne le 401 que §5.3 est fait pour rendre. Aucune des deux ne peut produire un tick vert qui ne veut rien dire, ce qui est la propriété que D7 et D8 réclament partout ailleurs.

**D13 — l'image de dev ne porte pas de quoi construire M5.** Tauri v2 se lie sous Linux à `webkit2gtk-4.1`, `javascriptcoregtk-4.1`, `libsoup-3.0` et `gtk+-3.0`, plus `libayatana-appindicator3-dev` pour le tray, `librsvg2-dev` et `patchelf` pour l'empaquetage. Aucune des quatre premières n'est dans `che-min-mise`. La conséquence n'est pas « le build échoue » mais quelque chose de plus large : **la coquille ne peut pas être compilée du tout**, donc rien de M5 ne peut être vérifié ici — pas même à la relecture d'un `cargo check`. Écrire un milestone entier dans ces conditions produirait la plus grande surface non vérifiée du projet, très au-delà de D12 qui compte deux lignes.

*Solution* : ajouter les paquets à `batleforc/WeeboDevImage` — c'est là qu'ils appartiennent, et §3 le dit déjà pour tout le reste (une amélioration générique remonte dans le template plutôt que d'être contournée dans un dépôt). Ce dépôt-ci n'a rien à changer : le container `tools` hérite du parent, donc un tag reconstruit est tout le correctif, et `devfile.yaml` reste tel quel.

En attendant, la question se pose à la machine et pas au document : `task tauri:doctor` liste ce qui manque et où le réparer. Une commande plutôt qu'un paragraphe, pour la raison habituelle ici — une phrase écrite devient fausse le jour où quelqu'un corrige son environnement, un contrôle qui s'exécute jamais.

**Vu — `che-mise-webkit` a remplacé `che-min-mise` comme parent du devfile, et `task tauri:doctor` rapporte les quatre bibliothèques présentes.** Le premier `cargo check` réel de `web/src-tauri` — jamais tenté avant cette session — a trouvé exactement une erreur : un `use tauri_plugin_deep_link::DeepLinkExt;` manquant, celui que `app.deep_link()` demandait déjà sans l'avoir. Corrigée, `task tauri:build` produit pour la première fois un binaire desktop, et le tray — jusque-là deux fonctions mortes sous `#[allow(dead_code)]` — est un vrai `TrayIconBuilder` avec un menu, un état vivant lu depuis `notify-client-core` par le même montage que le TUI, et la fermeture qui réduit dans le tray plutôt que de quitter. Détail dans M5 ci-dessus et dans `web/src-tauri/README.md`, qui tient à jour ce qui reste faux dans cette moitié désormais construite.

**D16 — §13 a une puce « Traces » que rien n'honore.** M4ter a livré les métriques et la moitié qui reste est nommée ici plutôt que laissée à la lecture optimiste d'un titre de section coché : il n'y a **aucune dépendance OpenTelemetry dans le workspace**, aucun `#[instrument]` sur un use case, et rien qui parle à un collecteur. `tracing` sert à émettre des événements structurés, ce qui donne des logs — pas des traces. La propagation W3C `traceparent` en entrée, celle qui permettrait à une CI de rattacher sa trace à la notification qu'elle publie, n'existe donc pas non plus.

Le coût est celui d'un incident : `publish → fanout → transport push` traverse trois couches et une boucle de fond, et sans span la reconstitution se fait en corrélant des lignes de log à la main. C'est supportable à un replica, et ça ne l'est plus au troisième.

*Solution, faite* : **M4quater**. `tracing-opentelemetry` + un exporteur OTLP/HTTP dans la racine de composition, éteint par défaut et allumé par `[telemetry]` — l'endpoint du collecteur est un fait de déploiement, pas une constante. Les spans se posent sur les use cases, c'est-à-dire à l'endroit exact où M4ter dit de ne **pas** mettre les métriques, et cette différence est ce que les deux incréments ensemble rendent lisible : une latence de processus se mesure là où le processus agit, une trace suit une **requête** et naît donc là où elle entre. Le `traceparent` entrant se lit dans la même passe, et il est cru — c'est une corrélation, pas une preuve.

Deux écarts avec le texte ci-dessus, tous deux constatés en le lançant plutôt qu'en le relisant : le client HTTP de l'exporteur doit être le **bloquant** (le thread du processeur de batch n'est pas un runtime Tokio, et l'asynchrone y panique au premier envoi), et la propagation **sortante** n'est pas prise — un webhook de sink part sans `traceparent`, ce qui est nommé en fin de M4quater plutôt que sous-entendu.

**D17 — personne ne sert ce produit.** Découvert en cherchant où mettre la CSP de §10.3, et le trou est plus large que la CSP : **il n'y a aucun Containerfile dans ce dépôt**. `.forgejo/workflows/release.yaml` le dit lui-même — son étape « Build the server image » émet `::warning::Image build is not wired yet — it lands with the Containerfile in M1` —, et M1 est marqué ✅ commité en §14.1. Trois conséquences qui ne se voient pas au même endroit :

- **la release ne publie pas d'image**, donc un tag ne produit rien de déployable ;
- **deux en-têtes de sécurité n'ont pas d'émetteur** : `frame-ancestors` et `report-to` sont ignorés dans un `<meta>` (ADR-0019), donc rien n'empêche l'application d'être mise dans une iframe par une autre origine, et une violation de CSP est un message dans la console de quelqu'un plutôt qu'un signal reçu. `Strict-Transport-Security` est dans la même catégorie ;
- ~~**aucun job de CI ne lance `pnpm -C web build`**~~ — soldé dans le même incrément : `task build:web:smoke` tourne dans le job `check`, avec les valeurs de `web/.env.example`, parce que c'est le build qui stampe la politique et vérifie ses hashes contre la page réellement écrite. Il produit sciemment un bundle non déployable : un bundle est bâti pour un déploiement, donc la CI ne peut pas bâtir le vrai — elle peut seulement prouver que le pipeline s'exécute. `task build:web` est l'autre moitié, celle qui refuse de construire sans les trois variables.

*Solution* : l'empaquetage, en une fois. Un Containerfile multi-étages pour le serveur, un pour le bundle web avec ses en-têtes, l'étape image de `release.yaml` câblée dessus (multi-arch, cosign, provenance — ce que §3.2bis promet déjà), et la cible ajoutée au scan `trivy` de `task audit`. **Rien de tout ça n'est vérifiable dans cet environnement** : aucun moteur de conteneurs ici (§12.4, la raison de D13), donc l'écrire d'ici produirait la deuxième plus grande surface non vérifiée du projet. C'est un incrément à part entière et il se joue là où une image peut être construite.

**D32 — la moitié entrante que §7.1bis promet et que M6 ne pose pas.** La phrase existe deux fois dans le document : « UnifiedPush garde la boucle 100 % self-hosted sur Android (le serveur expose lui-même le endpoint push UnifiedPush, le distributeur vivant côté device §10bis.4) », et le tableau de §7.1 qui la résume en une ligne. Ce que M6 a écrit est la moitié dont `notify-push-webpush` donne déjà la forme — le fan-out **sort** vers l'endpoint qu'un device a fourni à `POST /devices` — et rien de plus : pas de route publique, pas de token opaque par device, pas de quota §8.2 sur un chemin qui n'existe pas.

**Pourquoi ce n'est pas un manque caché.** `notify-push-unifiedpush`, en tête de son propre fichier, refuse explicitement d'inventer cette route : un endpoint entrant n'a de client que le distributeur **embarqué** de §10bis.4, qui est lui-même du Kotlin, écrit contre une route Rust réelle plutôt que contre un paragraphe de rationale. Poser la route ici, sans rien pour l'appeler, produirait exactement le défaut que D14 et D18 ont nommé chacun une fois — un port complet dont rien n'atteint la porte — sauf que celui-ci le saurait d'avance.

**Ce que ça coûte aujourd'hui, précisément.** Rien pour un device qui utilise un distributeur **système** (ntfy et les autres) : ce transport leur parle déjà, à leur URL, sans que ce dépôt héberge quoi que ce soit. Le coût est pour le chemin « 100 % self-hosted » que §7.1bis vante comme l'alternative par défaut — sans l'endpoint entrant, un device sans distributeur système installé n'a nulle part où pousser, et la seule voie self-hosted que §10bis.4 promet n'existe pas encore côté serveur.

*Solution* : la route, écrite contre §8 plutôt que contre le paragraphe de §7.1bis — méthode, auth par token opaque révocable avec le device (§8 `DELETE /devices`), quotas de §8.2 — au moment où M6 reprend côté app, puisque c'est ce Kotlin qui en sera le seul appelant et donc le seul juge de sa forme réelle. Écrire la route avant lui serait deviner une API contre un client qui n'existe pas.

**D33 — l'image de dev ne porte pas le SDK/NDK Android.** Même forme que D13, un cran plus loin : Tauri v2 mobile a besoin d'un JDK, du SDK Android (`platform-tools`, `platforms;android-35`, `build-tools;35.0.0`) et du NDK (`clang` par triple cible, ce qui lie `notify-client-core` — `rusqlite` en feature `bundled` demande déjà un compilateur C pour un client, §9.3bis) pour que `cargo tauri android init`/`build` produisent quoi que ce soit. Rien de tout ça n'est dans `che-min-mise`, et rien ne peut l'y mettre depuis ce workspace, pour la raison que D13 a déjà établie : le pod entier est sans root, donc `apt-get` échoue aussi bien dans un sidecar que dans `tools`.

Le correctif n'est cependant pas la même forme que celui de D13. Le JDK est un paquet `apt` ordinaire et rejoint le même bloc que les sept paquets webkit. Le SDK et le NDK ne le sont pas : Google les distribue en zip licencié, installé par son propre `sdkmanager`, avec une licence à accepter — aucun `apt-get install platforms;android-35` n'existe. Le correctif est donc un script d'image, pas une ligne de plus dans une liste de paquets.

*Solution* : ajouter le JDK et le script SDK/NDK à `batleforc/WeeboDevImage` — le diff exact, versions incluses, est dans `docs/upstream/che-min-mise-android.md`, écrit avant que quiconque en ait besoin, comme celui de D13 l'a été avant `che-mise-webkit`. Ce dépôt-ci n'a rien à changer une fois le tag reconstruit : le container `tools` hérite du parent, comme pour D13. Ce que cette entrée corrige n'est **que** la question « peut-on construire le Kotlin » — D32 reste une question à part, celle de savoir quelle route ce Kotlin appellera, et elle attend délibérément ce Kotlin plutôt que l'image.

**D34 — aucun runner macOS dans l'infra self-hosted.** Le Swift de M7 — l'extension de notification qui déchiffre avant affichage, l'enrôlement Keychain (§10bis.5), le build iOS entier — a besoin de Xcode. Rien de ce que D13 et D33 ont déjà établi ne s'applique de la même façon ici : ce n'est pas une bibliothèque ou un SDK absent d'une image Linux, c'est un système d'exploitation qu'Apple ne laisse tourner que sur son propre matériel, licence et technique confondues. Aucune image `batleforc/WeeboDevImage`, aussi complète soit-elle, ne peut faire tourner Xcode dans ce pod.

**Pourquoi ce n'est pas D33 avec un autre nom.** D33 a un correctif écrit et prêt à appliquer (`docs/upstream/che-min-mise-android.md`) — la question n'est que « quand », pas « comment ». D34 n'a pas d'équivalent : il n'existe pas de diff de Containerfile qui résout « faire tourner macOS sur du matériel qui n'en est pas ». Les deux décisions ont la même forme en surface — « ce workspace ne peut pas construire la moitié plateforme de ce milestone » — et une différence qui compte : l'une se répare avec du texte dans ce dépôt, l'autre se répare avec une machine que ce dépôt ne peut pas décrire.

**Ce que la moitié serveur a quand même pu faire, et pourquoi c'était possible malgré D34.** `notify-push-apns` n'avait besoin ni de Kotlin, ni de Swift, ni de rien de bloqué par D34 : un appelant réel existe déjà (`push_transports()`, le même seam que Web Push et UnifiedPush utilisent), l'API qu'il appelle est publique et documentée par Apple plutôt que devinée, et rien n'exige un appareil réel pour être testé — signature ES256, scellement, construction d'enveloppe, tout se vérifie sans jamais adresser `api.push.apple.com`. C'est exactement le raisonnement qui a laissé M6 écrire `notify-push-unifiedpush` malgré D33 : D32 avait raison de refuser une route devinée contre un client qui n'existe pas, mais un transport sortant contre une API stable n'est pas ce cas-là.

*Solution* : aucune que ce dépôt puisse écrire. Le correctif est une décision d'infrastructure — un runner macOS hébergé (GitHub Actions en propose un) ou un Mac que le projet possède et opère lui-même — pas un changement de code, et §10bis.6 le dit déjà : c'est « le seul job qui ne tourne pas sur l'infra self-hosted » (§15), une exception nommée plutôt que découverte le jour où quelqu'un cherche pourquoi CI n'a pas de jambe iOS. Cette entrée reste ouverte jusqu'à ce que cette machine existe ; rien ici ne la ferme en écrivant plus de Rust.

---

## 15. Points de vigilance

- **Cluster = frontière de confiance** (§6) : le contenu est lisible par le serveur et circule en clair applicatif à l'intérieur du cluster (TLS terminé à l'ingress, trafic interne pod↔PG↔bucket, attachments non chiffrés applicativement §6.1) ; la sécurité repose sur le durcissement de l'infra (hardening plan : network policies, bucket privé, disques et backups chiffrés). À documenter explicitement pour les utilisateurs.
- **Au repos = prérequis d'exploitation, pas une option** : aucun chiffrement applicatif (décision §6.1) — la confidentialité d'un dump, d'un backup ou d'un disque repose **entièrement** sur le chiffrement disque et le chiffrement des backups (Postgres **et** bucket, y compris les backups logiques type `pg_dump` qui sortent du disque chiffré). À vérifier dans le hardening plan et à documenter en gras pour qui self-host ; si une exigence de conformité impose du chiffrement applicatif un jour, il revient via le port `KeyProvider` (§6.1).
- **RustFS est jeune** : API S3 suffisante pour presign + get/put, mais le contrat est « S3-compatible », pas « RustFS » — l'adapter `attachments-s3` ne doit utiliser que des opérations S3 standard pour pouvoir basculer sur MinIO/Garage/S3 sans code.
- **La compat IdP est un contrat testé, pas une promesse** — et le filet n'est pas encore tendu. Le dev quotidien tourne sur un mock que **nous écrivons** (§12.1, ADR-0014 — Authentik, ~1,5 Gi avec worker + redis, reste derrière `task dev:auth`), donc un écart de comportement entre mock et vrai IdP ne se voit pas au quotidien, et depuis ADR-0014 l'écart peut venir de nos propres hypothèses autant que de celles d'un tiers. La matrice `oidc-compat` (§3.2, T29) est censée être la **seule** lecture indépendante d'OIDC que le projet rencontre — **ne jamais merger un changement du flow auth sans elle**.

  **État réel, découvert en finissant M1** : la matrice annonçait trois fournisseurs et n'en testait aucun. Elle se déclenchait sur des chemins qui n'existent pas (`crates/notify-app/src/auth/**`, `tests/auth/**`), lançait `task test:e2e` avec des arguments que la tâche ne relaie pas, sans conteneur de fournisseur ni base de données — donc une sélection de tests qui se sautaient tous, et un tick vert signifiant « rien n'a tourné ». Aucune configuration Dex n'existe d'ailleurs dans le repo.

  Ce qui tourne aujourd'hui : la suite réelle (`crates/notify-adapters/oidc/tests/oidc_compat.rs`) prend un jeton d'un fournisseur **vivant** et le passe au vrai `OidcVerifier`, sur le leg `mock`. Elle est déjà paramétrée par `WNK_TEST_OIDC_PROVIDER`, et **échoue** si on nomme un fournisseur qu'elle ne peut pas joindre plutôt que de se sauter : c'est la propriété qui empêche le mode d'échec ci-dessus de revenir. La config Dex existe désormais dans le dépôt et les deux legs externes sont câblés en CI — mais **aucun des deux n'a jamais tourné**, faute de moteur de conteneurs ici : D7 en §14.4 dit exactement ce qui est vérifié et ce qui ne l'est pas.

  `TokenVerifier` in-memory reste le niveau zéro pour le travail domaine (`task dev:core`).
- **iOS** : APNs impose un compte Apple Developer et l'extension Swift ; c'est le seul point non 100 % self-hosted.
- **Rotation device** : la révocation d'un device doit invalider ses subscriptions push et son curseur de sync (et sa clé publique X25519 pour les devices mobiles).
- **Web Push = confiance dans le navigateur** : côté PWA la protection contre le push service repose sur RFC 8291, clés gérées en interne par le navigateur (décision §6.2 — aucune clé applicative côté web). C'est équivalent en pratique au niveau mobile, mais la garantie dépend de l'implémentation du navigateur, pas de nous — à documenter pour les canaux sensibles.
- **Avatar = seule surface de décodage média du serveur** (§2.4) : parsing d'images fournies par les users — décodeur sûr, plafond de dimensions avant décodage complet (anti-bombe de décompression), formats animés refusés, métadonnées EXIF strippées (GPS). Toute nouvelle feature qui ferait décoder du média côté serveur doit repasser par cette case.
- **Un webhook est une API publique et une sortie de la frontière de confiance** (§7.1bis) : le payload part en clair chez quelqu'un d'autre, donc un sink est une décision de confidentialité au même titre qu'un membership — c'est la config (§11) qui la prend, jamais l'API. Le secret HMAC vit en mémoire du process et **jamais dans la file** (ADR-0018) ; sa rotation est un `secret_ref` changé et un redémarrage, sans reprise de données. À documenter pour qui self-host : déclarer un sink sur un canal sensible, c'est autoriser un tiers à lire ce canal.
- **Clés de signature de release** (§3.2bis) : la clé privée minisign de l'updater est un équivalent « RCE sur tout le parc desktop » en cas de fuite — custody stricte : secrets CI Forgejo scopés au seul workflow release, jamais dans le repo, rotation documentée. Même exigence pour cosign, le keystore de signature Android et la clé APNs `.p8`.
- **Le « feeling natif » dépend du moteur webview** (§10bis.3bis) : Windows exige le runtime WebView2 (bootstrapper embarqué dans le `.msi`) ; Linux dépend de la version webkit2gtk de la distro — rendu et comportements à valider sur les cibles réelles, pas seulement sur la machine de dev.
