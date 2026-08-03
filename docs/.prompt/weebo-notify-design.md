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
task init          # mise upgrade + cog install-hook -a -o
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
- **T21 · Réconciliation GitOps** : canal retiré de la config → archivé/supprimé selon `on_removed` ; drift (TTL modifié en base) corrigé au SIGHUP ; canal `dynamic` jamais touché (§11).
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
- Refresh silencieux par **rotation du refresh token** (Authentik) ; tokens gardés hors du localStorage (mémoire + refresh piloté par le service worker).

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
- Headers : `X-Weebo-Event` (type d'événement), `X-Weebo-Delivery` (UUID unique — clé d'idempotence côté consommateur), `X-Weebo-Signature: t=<unix_ts>,v1=<hex(hmac_sha256(secret, t + "." + body))>`.
- Anti-replay : la signature couvre le timestamp ; le consommateur rejette si `|now − t| > 5 min`.
- Retries : backoff exponentiel 30 s → 1 h, abandon après 24 h → **DLQ** (métriquée §13, requeue manuel via `POST /admin/dlq/{id}/requeue` §8 — UI dans la console plateforme §10.4bis).

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
- Purge : job périodique côté serveur (soft-delete → hard-delete) ; les révisions d'une notification expirent avec elle. Un message expiré non livré n'apparaît **pas** dans le backlog de sync — le client reçoit à la place un compteur « N notifications expirées pendant votre absence » par canal.
- La compaction s'applique avant le TTL : au sync, une notification à 40 révisions = 1 entrée.

### 7.3 Accusés (opt-in par canal)

- `delivered` (automatique, par device) et `seen` (interaction utilisateur).
- Si activé sur le canal : les membres autorisés voient « vu par 3/5 ».
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
GET    /api/v1/sync?cursor=...                # backlog offline
GET    /api/v1/stream                         # SSE (contrat §8.1)
GET    /api/v1/ws                             # WebSocket (mêmes événements, frames JSON)
POST   /api/v1/receipts                       # delivered/seen
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
POST   /api/v1/auth/token/exchange            # fédération JWT CI
POST   /api/v1/attachments/presign            # upload direct au bucket (TTL court, taille/type imposés §6.1)
GET    /api/v1/admin/audit                    # audit log browsable (filtres principal/action/période) — rôle §5.4 uniquement
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
notify device   enroll            # enregistre le device (curseur de sync) — zéro crypto : la CLI reçoit via SSE/TLS (§6.2)
notify token    exchange          # depuis un job CI
notify dnd      on|off|until 9h|for 2h|schedule|status   # DND serveur (§7.4) — schedule édite les quiet hours ; dispo dans le TUI via :dnd
notify completions <shell>        # complétions clap_complete
```

> Garde-fou non-interactif : si stdout n'est pas un TTY (pipe, cron, CI), `notify` sans sous-commande n'ouvre pas le TUI — il affiche l'aide et sort avec le code 2, pour ne jamais bloquer un script.

Conventions :
- **Config en couches** (même philosophie que le serveur §11) : défauts < `~/.config/weebo-notify/config.toml` < env `WNK_*` < flags clap. Un flag global `--profile` permet plusieurs serveurs (perso, boulot).
- **Sortie scriptable** : chaque commande accepte `--output json|table` ; les erreurs partent sur stderr avec exit codes stables (0 ok, 1 erreur API, 2 usage) — utilisable en CI sans parsing fragile.
- Les valeurs contraintes (`--status`, `--state`, `--priority`) sont des enums clap (`ValueEnum`) dérivées des types de `notify-domain` → CLI et serveur ne peuvent pas diverger.
- `publish`/`update` lisent aussi le body sur stdin (`-` ou pipe) pour les usages `cmd | notify publish -c ...`.
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
- **Passthrough notifications système** : en plus du rendu TUI, option `--notify-send` qui relaie vers les notifications desktop (notify-rust) quand le terminal n'a pas le focus.
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
- **Rendu markdown sanitizé** (§2.3) : markdown pur, HTML brut jamais interprété, liens `http(s)` uniquement, CSP stricte sans `unsafe-inline` — le contenu d'un canal peut venir de publishers moins fiables que ses lecteurs.
- **Actions** (§2.3) : boutons shadcn `Button` en pied de card — `view` = variante `link`/`outline`, `ack` = `default` (état acquitté persistant), `reply` = `ghost` + input inline, `choice` = groupe de boutons `outline` (l'option retenue passe en `default`), `silence` = `ghost` + menu durée/scope (`DropdownMenu`). Receipts « vu par 3/5 » — le compte est en **users** (§7.3, `seen` est une interaction utilisateur) — en `HoverCard` listant qui a vu et quand ; le `delivered` par device reste un détail replié.
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
- CI (§3.2) : jobs desktop sur les trois OS (artefacts de release), Android en conteneur (SDK/NDK pinnés via mise), iOS sur runner macOS — le seul job qui ne tourne pas sur l'infra self-hosted (§15).

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

[push.apns]
key_file = "/secrets/apns.p8"
team_id  = "..."

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
  secret_ref = "env:WNK_SINK_INCIDENTS_HMAC"   # ou file:/... — résolu par le SecretResolver

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

---

## 13. Observabilité

- **Traces** : `tracing` + `tracing-opentelemetry`, export OTLP ; spans sur chaque use case (publish → fanout → push transport), propagation W3C traceparent acceptée en entrée (une CI peut lier sa trace à la notification).
- **Metrics** : exporter Prometheus `/metrics` — `notifications_published_total{channel}`, `push_delivery_duration_seconds{transport}`, `push_failures_total{transport,reason}`, `sync_backlog_size`, `realtime_connections{transport="sse"|"ws"}` (jauge — sert aussi à l'HPA §11), `sink_dlq_depth{channel}` (la DLQ métriquée de §7.1bis).
- **Logs** : JSON structurés, jamais de payload.
- **Audit log** : les actions d'administration (création/révocation d'API keys, de devices et de service accounts, membership — self-join d'un `platform-admin` compris (§5.4) —, trust policies, requeue DLQ) sont journalisées dans une table **append-only** et émises en logs structurés `audit=true` — flux séparé des logs techniques : c'est celui qu'on rejoue après un incident. Browsable via `GET /admin/audit` (§8) dans la console plateforme (§10.4bis).
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

### 14.1 État d'avancement

> Mis à jour au fil de l'eau. « écrit » veut dire *dans l'arbre de travail et testé* ;
> « commité » veut dire *dans l'historique*. La distinction compte : un incrément non
> commité n'a pas passé les hooks (`cog verify`, `task recu`, `task lint`, gitleaks).

| Milestone | État |
|---|---|
| **M0 — Socle** | ✅ commité (`240ff36`) |
| **M1 — Cœur + web v0** | 🟡 **écrit en entier**, pas encore commité — détail ci-dessous |
| **M2 — Sync, offline & TUI** | 🟡 **écrit en entier**, pas encore commité — les huit incréments et les quatre manques soldés |
| M3 → M7 | ⬜ non commencés |

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

Une verrue relevée en câblant : la spec **inline le schéma des items** dans `Page_Channel` et `Page_Notification` au lieu de référencer `Channel` et `Notification`. Le client TS s'en moque (typage structurel) mais progenitor y frappe un second type nominal — `PageChannelItemsItem` — pour la même chose. `notify-client-core` l'isole derrière un alias pour que la verrue n'atteigne ni la CLI, ni le TUI, ni Tauri ; le vrai correctif est dans la façon dont utoipa compose `Page<T>`, et il reste à faire.

**M2.6 à M2.8 en détail.** Le TUI est en place — architecture Elm-like (un `App`, un enum `Message`, un `update` seul autorisé à muter, un `draw` sans effet de bord), layout 3 zones miroir du web, hook panic qui restaure le terminal **avant** de crasher, et chaque chaîne venue du serveur strippée de ses séquences ANSI avant rendu (§2.3 — le serveur strippe déjà à l'ingestion, mais ici le terminal *est* le moteur de rendu). Le mode `:` passe la ligne au **même parseur clap** que le shell, ce qui est le mécanisme de parité de §9 : une sous-commande ajoutée à la CLI y est disponible sans une ligne de code TUI.

Le sync client remplit le store depuis `GET /sync` en suivant le curseur, et remonte les deux compteurs que §7.2/§7.2bis rendent visibles : les révisions repliées par la compaction, et les notifications expirées par canal pendant l'absence.

Côté web, Settings et Administration. Settings existe pour rendre visible ce que l'API modélise et qu'aucun autre client ne montre : **quels champs t'appartiennent**, avec un bouton pour les rendre à l'IdP. Administration met canaux, devices, clés et service accounts sur un écran parce que c'est une seule question — *qu'est-ce qui peut agir en mon nom, et sous quelles règles* — et le formulaire de clé n'offre **que** des propriétaires service account, ce qui est §5.2 transformé en interface plutôt qu'en documentation.

L'écran des canaux ne décide pas qui a le droit d'éditer une policy : §5.1 dérive l'autorisation par requête, donc l'interface propose et affiche le refus s'il vient. Ce qu'elle cache est ce que la **donnée** tranche : un canal `managed` n'a aucun contrôle du tout, parce que la config le possède (§11) et que la prochaine réconciliation annulerait l'appel.

**Trois trouvailles, dont une grave :**

- **`task recu` était cassé et ma vérification ne le voyait pas.** L'upload d'avatar de M2.2 déclarait son corps en `Vec<u8>`, qu'utoipa rend en tableau d'entiers et que progenitor refuse pour `application/octet-stream`. La génération du client Rust échouait donc — et mon contrôle d'idempotence (« les fichiers n'ont pas changé ») **passait justement parce que rien ne tournait**. Exactement le mode d'échec dénoncé plus haut dans ce document, reproduit par moi. Le corps se déclare maintenant `string`/`format: binary`, et la règle qui manquait est écrite en D8 (§14.4) : une vérification doit prouver que le travail a eu lieu avant de vérifier son résultat.
- **Le client Rust ne peut pas consommer `GET /sync`.** progenitor rend `Event` — un `allOf` de deux `oneOf`, ce qui produit la forme plate de §8.1 — en `pub enum EventVariant0 {}`, un type **inhabité** : la désérialisation ne marche que tant qu'il n'y a aucun événement. `notify-client-core` lit donc la réponse avec son propre modèle minimal, exception documentée dans le module. Le correctif est D1 en §14.4.
- **La spec inline les items paginés** au lieu de référencer `Channel`/`Notification`, ce qui fait frapper à progenitor un second type nominal pour la même chose (M2.5).

**Fermer M2 — les quatre manques soldés.** Le signal local de §9.3bis, l'exécution des commandes `:`, le badge retard et l'admin canaux étaient nommés ici plutôt que passés sous silence ; ils sont faits, et §14.4 dit lesquels par leur numéro (D2, D4, D5, D6). Deux choses n'étaient pas au programme et sont sorties en les faisant :

- **Un refus « managed » rendait 422 alors que §8 documentait 409 sur les trois routes concernées.** `Origin::ensure_mutable_through_api` refusait avec un `Malformed`, donc le code stable sur le fil était `field.malformed` : un client qui branche dessus aurait traité « va changer la config » comme « tu as mal tapé ». La requête n'a rien de malformé — elle est comprise, et refusée par une question de propriété. D'où un variant à part, `origin.managed`, de catégorie `Conflict`. Le document ne mentait pas ; c'est le code qui ne le tenait pas.
- **Le store local ne gardait que le *contenu* d'une révision**, pas l'événement. Une lectrice ne pouvait donc rien rendre depuis le cache — ni statut, ni état, ni horodatage —, ce qui rendait le signal local inutilisable pour ce à quoi §9.3bis le destine. Le cache stocke maintenant l'événement **verbatim** (ADR-0013 : la forme sur le fil *est* le contrat) et en relit ce qu'il dessine, donc un build ultérieur peut afficher ce que celui-ci a mis de côté.

Deux formes valent d'être notées parce qu'elles se ressemblent et se décident autrement :

- **le plafond de TTL se retire en écrivant `"0"`, pas `null`.** §7.2bis donne déjà à `"0"` le sens « illimité » partout où une durée apparaît, et un plafond *à jamais* **est** l'absence de plafond : une seule orthographe. Le tri-état de §2.4 (absent ≠ `null`) aurait été le réflexe, et il aurait été faux ici pour une raison mécanique — progenitor rend tout champ optionnel avec `skip_serializing_if`, donc le client Rust sait omettre une clé mais jamais l'envoyer nulle. ADR-0009 promet que les clients viennent du contrat ; le contrat n'a donc pas le droit de décrire ce qu'ils ne savent pas dire. Le profil garde son `null` parce que rien de généré n'écrit encore ce corps-là — le jour où ça arrive, D-profil rejoint cette liste ;
- **`allow_forever` reste hors du `PATCH`.** §7.2bis en fait une décision d'opérateur : la laisser passer par l'API rendrait la réponse de la config consultative.

Et une règle appliquée à la présentation plutôt qu'aux règles métier : **chaque commande CLI renvoie une `String` au lieu d'imprimer**. C'est ce qui permet au mode `:` d'*exécuter* les commandes et d'afficher **la réponse du shell**, pas une seconde mise en forme de la même chose — la §9.3 (« toute logique qui apparaît dans `notify-cli` doit redescendre ») vaut aussi pour ce qu'on écrit à l'écran, parce que deux rendus divergent et que la divergence est invisible tant que personne ne compare deux écrans.

Portes au vert à la fin de M2 : `task lint:check`, `task test` (479 Rust + 52 web), `task test:critical` (73), `task test:e2e` (47, contre un vrai Postgres — vérifié en comptant zéro « skipping » et un temps d'exécution qui n'est pas celui d'une suite qui se saute), `task test:oidc-compat` (leg `mock`), `task audit` (cargo + deny + pnpm + trivy), couverture domaine+app à 96,3 % pour un plancher à 90 %, et `task recu` idempotent — l'idempotence contrôlée **après** avoir vérifié que la génération avait bien tourné, ce que D8 exige et ce que la fin de M2 avait raté.

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
| `PushDispatcher` | ✅ | ✅ `TransportRegistry` (M1.8) | Vide en M1, ce qui est le comportement correct : `Fanout` saute déjà les devices `stream`, donc le registre n'est atteint que par un device enrôlé sur un transport que cette instance ne sert pas — il répond `NotConfigured` en nommant la capacité. |
| `AttachmentStore` | ✅ | ✅ `S3Attachments` (M2.2) | Conformité passée contre un vrai RustFS. Un déploiement sans `[attachments]` reçoit un double qui **refuse** (`NotConfigured`) au lieu de réussir dans le vide. |
| `AvatarProcessor` | — | ✅ `notify-avatars` (M2.2) | Seule surface de décodage média du serveur (§15) : taille, format d'après les octets, dimensions **avant** décodage, ré-encodage qui strippe les métadonnées. |
| `SinkDispatcher` / `SinkQueue` | ✅ | ⬜ | Hors périmètre M1 (§7.1bis). |
| `FederationVerifier` | ❌ | ⬜ | M4 (§5.3). Pas encore de double non plus. |
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
| D1 | `Event` illisible par le client Rust | `GET /sync` inutilisable depuis progenitor | M | ⬜ ouverte |
| D2 | `PATCH /channels/{id}` absent | — | S | ✅ soldée en fermant M2 |
| D3 | Items paginés inlinés dans la spec | types dupliqués dans le client Rust | S | ⬜ ouverte |
| D4 | Commandes `:` non exécutées | — | M | ✅ soldée en fermant M2 |
| D5 | Signal local absent (§9.3bis) | — | M | ✅ soldée en fermant M2 |
| D6 | `delivered_at` non conservé | — | S | ✅ soldée en fermant M2 |
| D7 | Legs Dex et Authentik de `oidc-compat` | une seule lecture d'OIDC au lieu de trois (§15) | M | ⬜ ouverte |
| D8 | Une vérification qui passe sans rien exécuter | — | S | ✅ règle appliquée |

Les quatre entrées soldées gardent leur texte ci-dessous plutôt que d'être effacées : une dette dont on ne peut plus relire la raison redevient un souvenir, et c'est exactement ce que cette section refuse.

**D1 — `Event` rendu en type inhabité.** progenitor voit `allOf[Audience(oneOf), EventPayload(oneOf), {cursor, logged_at}]` et produit `pub enum EventVariant0 {}` : la désérialisation d'une page de sync ne marche que tant qu'elle est vide. `notify-client-core` contourne avec son propre modèle minimal (documenté dans le module), donc rien n'est cassé — mais la promesse d'ADR-0009, « les clients viennent du contrat », a un trou.

*Solution* : **décrire `Event` à la main plutôt que le laisser composer**. Un `impl ToSchema` qui émet un `oneOf` de cinq objets complets — un par `event`, chacun portant déjà `channel_id` ou `user_id`, `cursor` et `logged_at`. **La forme sur le fil ne change pas d'un octet** ; seule sa description change, et elle devient quelque chose qu'un générateur sait modéliser. Le coût est un schéma écrit à la main qui doit suivre le type : à couvrir par un test qui sérialise un événement de chaque genre et vérifie que les champs obligatoires du schéma correspondent. À faire avant que quoi que ce soit d'autre consomme `/sync` en Rust — le TUI le fera.

**D2 — pas de mise à jour de canal.** §9.1 promet `notify channel set-ttl` et §10.4 un écran d'admin canaux, alors que §8 n'avait aucune route pour changer une policy. L'incohérence était dans la spec, pas dans le code ; §8 porte désormais la route.

*Solution, faite* : `PATCH /api/v1/channels/{id}`, corps = policy partielle (`default_ttl`, `max_ttl`, `default_priority`, `receipts_enabled`), qui **refuse un canal managé** comme `DELETE` le fait déjà, exige le scope `manage`, et s'inscrit à l'audit avec la policy **résultante** — un log d'audit se relit pour savoir à quoi c'était réglé, et un corps partiel ne répond pas à cette question. La CLI (`channel set-ttl`) et l'écran web en découlent.

Trois décisions prises en l'écrivant :

- **le domaine refuse une policy sous laquelle le canal ne saurait plus publier.** Après application, `TtlPolicy::resolve(None)` est rejoué et son refus est renvoyé : un `default` à `forever` sur un canal qui ne l'autorise pas ferait échouer *chaque* publication à venir, très loin de l'édition qui l'a causé. C'est la vérification faite là où l'erreur est faite ;
- **un `PATCH` qui ne change rien n'écrit rien.** `edit_policy` répond si quelque chose a bougé ; sinon la réponse est la même — un `PATCH` est idempotent — mais sans ligne en base ni entrée d'audit affirmant un changement qui n'a pas eu lieu ;
- **le refus « managed » est un 409**, ce qu'il n'était pas (voir les trouvailles de M2.6–M2.8).

**D3 — items paginés inlinés.** `Page_Channel.items` décrit l'objet au lieu de référencer `Channel`, donc progenitor frappe `PageChannelItemsItem` — un second type nominal pour la même chose. Le client TS s'en moque (typage structurel), le Rust non ; `notify-client-core` masque la verrue derrière un alias pour qu'elle n'atteigne pas les frontales.

*Solution* : un `impl PartialSchema`/`ToSchema` manuel pour `Page<T>` où `items` est un tableau de **référence** vers `T`, au lieu de laisser utoipa inliner le paramètre générique. Effet de bord agréable : le document rétrécit d'une copie complète de `Channel` et de `Notification`.

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

**D8 — un contrôle vert qui ne prouvait rien.** En écrivant M2, `task recu` échouait (D-avatar, corrigé) et le contrôle d'idempotence — « les fichiers générés n'ont pas bougé » — **passait justement parce que la génération n'avait pas tourné**. C'est le même mode d'échec que D7 sous une autre forme, et il a été commis ici après avoir été dénoncé deux sections plus haut.

*Solution* : la règle, écrite pour qu'elle serve ailleurs — **une vérification doit prouver que le travail a eu lieu avant de vérifier son résultat**. Concrètement : contrôler le code de sortie *et* une trace d'exécution (`task recu` nomme ce qu'il régénère) avant de comparer des empreintes. Le job `contract` en CI était protégé par son code de sortie ; c'est la vérification manuelle qui ne l'était pas.

---

## 15. Points de vigilance

- **Cluster = frontière de confiance** (§6) : le contenu est lisible par le serveur et circule en clair applicatif à l'intérieur du cluster (TLS terminé à l'ingress, trafic interne pod↔PG↔bucket, attachments non chiffrés applicativement §6.1) ; la sécurité repose sur le durcissement de l'infra (hardening plan : network policies, bucket privé, disques et backups chiffrés). À documenter explicitement pour les utilisateurs.
- **Au repos = prérequis d'exploitation, pas une option** : aucun chiffrement applicatif (décision §6.1) — la confidentialité d'un dump, d'un backup ou d'un disque repose **entièrement** sur le chiffrement disque et le chiffrement des backups (Postgres **et** bucket, y compris les backups logiques type `pg_dump` qui sortent du disque chiffré). À vérifier dans le hardening plan et à documenter en gras pour qui self-host ; si une exigence de conformité impose du chiffrement applicatif un jour, il revient via le port `KeyProvider` (§6.1).
- **RustFS est jeune** : API S3 suffisante pour presign + get/put, mais le contrat est « S3-compatible », pas « RustFS » — l'adapter `attachments-s3` ne doit utiliser que des opérations S3 standard pour pouvoir basculer sur MinIO/Garage/S3 sans code.
- **La compat IdP est un contrat testé, pas une promesse** — et le filet n'est pas encore tendu. Le dev quotidien tourne sur un mock que **nous écrivons** (§12.1, ADR-0014 — Authentik, ~1,5 Gi avec worker + redis, reste derrière `task dev:auth`), donc un écart de comportement entre mock et vrai IdP ne se voit pas au quotidien, et depuis ADR-0014 l'écart peut venir de nos propres hypothèses autant que de celles d'un tiers. La matrice `oidc-compat` (§3.2, T29) est censée être la **seule** lecture indépendante d'OIDC que le projet rencontre — **ne jamais merger un changement du flow auth sans elle**.

  **État réel, découvert en finissant M1** : la matrice annonçait trois fournisseurs et n'en testait aucun. Elle se déclenchait sur des chemins qui n'existent pas (`crates/notify-app/src/auth/**`, `tests/auth/**`), lançait `task test:e2e` avec des arguments que la tâche ne relaie pas, sans conteneur de fournisseur ni base de données — donc une sélection de tests qui se sautaient tous, et un tick vert signifiant « rien n'a tourné ». Aucune configuration Dex n'existe d'ailleurs dans le repo.

  Ce qui tourne aujourd'hui : la suite réelle (`crates/notify-adapters/oidc/tests/oidc_compat.rs`) prend un jeton d'un fournisseur **vivant** et le passe au vrai `OidcVerifier`, sur le leg `mock`. Elle est déjà paramétrée par `WNK_TEST_OIDC_PROVIDER`, et **échoue** si on nomme un fournisseur qu'elle ne peut pas joindre plutôt que de se sauter : c'est la propriété qui empêche le mode d'échec ci-dessus de revenir. Câbler Dex et Authentik reste à faire — c'est D7 en §14.4, avec la marche à suivre.

  `TokenVerifier` in-memory reste le niveau zéro pour le travail domaine (`task dev:core`).
- **iOS** : APNs impose un compte Apple Developer et l'extension Swift ; c'est le seul point non 100 % self-hosted.
- **Rotation device** : la révocation d'un device doit invalider ses subscriptions push et son curseur de sync (et sa clé publique X25519 pour les devices mobiles).
- **Web Push = confiance dans le navigateur** : côté PWA la protection contre le push service repose sur RFC 8291, clés gérées en interne par le navigateur (décision §6.2 — aucune clé applicative côté web). C'est équivalent en pratique au niveau mobile, mais la garantie dépend de l'implémentation du navigateur, pas de nous — à documenter pour les canaux sensibles.
- **Avatar = seule surface de décodage média du serveur** (§2.4) : parsing d'images fournies par les users — décodeur sûr, plafond de dimensions avant décodage complet (anti-bombe de décompression), formats animés refusés, métadonnées EXIF strippées (GPS). Toute nouvelle feature qui ferait décoder du média côté serveur doit repasser par cette case.
- **Clés de signature de release** (§3.2bis) : la clé privée minisign de l'updater est un équivalent « RCE sur tout le parc desktop » en cas de fuite — custody stricte : secrets CI Forgejo scopés au seul workflow release, jamais dans le repo, rotation documentée. Même exigence pour cosign, le keystore de signature Android et la clé APNs `.p8`.
- **Le « feeling natif » dépend du moteur webview** (§10bis.3bis) : Windows exige le runtime WebView2 (bootstrapper embarqué dans le `.msi`) ; Linux dépend de la version webkit2gtk de la distro — rendu et comportements à valider sur les cibles réelles, pas seulement sur la machine de dev.
