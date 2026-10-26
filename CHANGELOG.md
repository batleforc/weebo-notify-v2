# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## [v0.1.2](https://github.com/batleforc/weebo-notify-v2/compare/a823442b3d51ec400d6d73f261b19edf3b09318c..v0.1.2) - 2026-10-26
#### Bug Fixes
- (**web**) export CSP origins - ([a823442](https://github.com/batleforc/weebo-notify-v2/commit/a823442b3d51ec400d6d73f261b19edf3b09318c)) - Max Batleforc

- - -

## [v0.1.1](https://github.com/batleforc/weebo-notify-v2/compare/0cc149be414fc230ab3493034bdd49d078f7f5be..v0.1.1) - 2026-10-26
#### Bug Fixes
- (**web**) allow OIDC token endpoint in CSP - ([0cc149b](https://github.com/batleforc/weebo-notify-v2/commit/0cc149be414fc230ab3493034bdd49d078f7f5be)) - Max Batleforc

- - -

## [v0.1.0](https://github.com/batleforc/weebo-notify-v2/compare/v0.1.0-rc.1..v0.1.0) - 2026-10-26
#### Features
- (**api**) GET /api/v1/client-config, what a native client needs to sign in - ([fad97cd](https://github.com/batleforc/weebo-notify-v2/commit/fad97cdb2dfb39b4244250d75763fdec2db2191c)) - Max Batleforc, Claude Opus 5.5
- (**client-core**) probe, check and keep the instance a shell talks to - ([20d9cb4](https://github.com/batleforc/weebo-notify-v2/commit/20d9cb473b5c1a3c7ee476d156bfbc1f344933bb)) - Max Batleforc, Claude Opus 5.5
- (**desktop**) a credential per instance, and a bundle that names none - ([779d40d](https://github.com/batleforc/weebo-notify-v2/commit/779d40dc8e52c93c783ed1c7493d90c1fb7e4a89)) - Max Batleforc, Claude Opus 5.5
- (**desktop**) the shell keeps the chosen instance and writes its CSP - ([5504f46](https://github.com/batleforc/weebo-notify-v2/commit/5504f46b216afd125a66fea034c63addb338de36)) - Max Batleforc, Claude Opus 5.5
- (**web**) the desktop app asks which server at first launch - ([87efc6c](https://github.com/batleforc/weebo-notify-v2/commit/87efc6c3387ea5816e7683514208eb4d758e2e99)) - Max Batleforc, Claude Opus 5.5
- wip - ([fb73ece](https://github.com/batleforc/weebo-notify-v2/commit/fb73eced0c480b754e5adf79ce69646580cc5932)) - Max Batleforc
#### Bug Fixes
- (**desktop**) end the loopback sign-in only on this attempt's callback - ([71f88eb](https://github.com/batleforc/weebo-notify-v2/commit/71f88ebbc1d27fe56d4dd0966dd79a33f90db9d5)) - Max Batleforc, Claude Opus 5.5
- (**web**) upgrade curl/libcurl in the web image for CVE-2026-92392 - ([dd0d4a5](https://github.com/batleforc/weebo-notify-v2/commit/dd0d4a5b7164bd8e09ca0bbd5484024cf1a8249c)) - Max Batleforc, Claude Opus 5.5
- (**web**) revoke the refresh token on sign-out; trust no callback before its state - ([06ff7ab](https://github.com/batleforc/weebo-notify-v2/commit/06ff7ab99551393b3486ce650e08516ee41af6f2)) - Max Batleforc, Claude Opus 5.5
#### Documentation
- (**rfc**) RFC 0001, the shell chooses its instance at first launch - ([5f5866f](https://github.com/batleforc/weebo-notify-v2/commit/5f5866f2d38d4ccfd23affc1a36ba791463cb406)) - Max Batleforc, Claude Opus 5.5
- (**security**) the device adversary, and what the updater signature leaves out - ([b6720e3](https://github.com/batleforc/weebo-notify-v2/commit/b6720e37a1c900d2ed8a230507b7377201a29223)) - Max Batleforc, Claude Opus 5.5
- (**todo**) what RFC 0001 still owes after phases 1 to 3 - ([ed8e984](https://github.com/batleforc/weebo-notify-v2/commit/ed8e984e24e1e1d7a24948ec2a6e19f00a215459)) - Max Batleforc, Claude Opus 5.5
- close SEC-P0-08/09 and TODO 8 after the first real release - ([e0b196f](https://github.com/batleforc/weebo-notify-v2/commit/e0b196fe888d54baf25b0e2fc6f31f1f6489ff0f)) - Max Batleforc, Claude Opus 5.5
#### Tests
- (**persistence-pg**) serialise the last fan-out claim test with the others - ([7a994bc](https://github.com/batleforc/weebo-notify-v2/commit/7a994bcab4be930cd83c9b67f4923bfea33fea18)) - Max Batleforc, Claude Opus 5.5
#### Continuous Integration
- (**desktop**) lint and test the Tauri crate on every change that reaches it - ([0e7a9fc](https://github.com/batleforc/weebo-notify-v2/commit/0e7a9fcd2548386f3340dcbc47f9fb9ed182a4b3)) - Max Batleforc, Claude Opus 5.5
- (**security**) add an authenticated ZAP pass to task audit:dast - ([5fdece0](https://github.com/batleforc/weebo-notify-v2/commit/5fdece037c9c10efcca2de12aff3b65407102b0f)) - Max Batleforc, Claude Opus 5.5
- (**security**) scan a running stack with OWASP ZAP (task audit:dast) - ([f542a48](https://github.com/batleforc/weebo-notify-v2/commit/f542a48a99642d65aaad65f203e503410ef44b1d)) - Max Batleforc, Claude Opus 5.5

- - -

## [v0.1.0-rc.1](https://github.com/batleforc/weebo-notify-v2/compare/v0.1.0-rc.0..v0.1.0-rc.1) - 2026-10-25
#### Bug Fixes
- (**authentik**) allow the grants the login and the compat suite use - ([2bbd361](https://github.com/batleforc/weebo-notify-v2/commit/2bbd361c1883232ff135f949f10153ad69229776)) - Max Batleforc, Claude Opus 5.5
- (**deps**) move crypto-bigint off its yanked 0.7.x releases - ([2becbd5](https://github.com/batleforc/weebo-notify-v2/commit/2becbd51581394c8488771659099941a935c6799)) - Max Batleforc, Claude Opus 5.5
- (**release**) sync the version into the fuzz and Tauri lockfiles - ([9593895](https://github.com/batleforc/weebo-notify-v2/commit/9593895e377e85c7268990ac9ce3826db68e9c46)) - Max Batleforc, Claude Opus 5.5
#### Miscellaneous Chores
- (**release**) leave CHANGELOG.md out of the clean-tree pre-hook - ([afbf0e1](https://github.com/batleforc/weebo-notify-v2/commit/afbf0e11c4497c875621dd55cf07a076845b0dfa)) - Max Batleforc, Claude Opus 5.5
- (**version**) v0.1.0-rc.1 - ([d838a63](https://github.com/batleforc/weebo-notify-v2/commit/d838a63f52cd82f1ef18b583d17c41f1e4b46ec8)) - Max Batleforc

- - -

## [v0.1.0-rc.0](https://github.com/batleforc/weebo-notify-v2/compare/976ebfe5dafeb3416ad00cf6335c1ed0bc11ff89..v0.1.0-rc.0) - 2026-10-25
#### Features
- sec review - ([385df89](https://github.com/batleforc/weebo-notify-v2/commit/385df895a956a40874df0af2c0107d5b00d00f98)) - Max Batleforc
- production readiness, BatleHub-style UI, ecosystem compat and device management (#3) - ([4a7e86a](https://github.com/batleforc/weebo-notify-v2/commit/4a7e86a347515c2da44660b0d591cf41c58c9403)) - Maxime, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude
- SECURE DA BOUZIN - ([e18c718](https://github.com/batleforc/weebo-notify-v2/commit/e18c718c6a136a5062cb9acf3fd256492928655f)) - Max Batleforc
- WIP (#2) - ([6463e12](https://github.com/batleforc/weebo-notify-v2/commit/6463e1229e57e52e20d5d9472ca40071fad384cf)) - Maxime
- socle projet weebo-notify (M0) (#1) - ([f1ec9ff](https://github.com/batleforc/weebo-notify-v2/commit/f1ec9ff6fb8a0e96c62c4567e41cf8b6da679ab3)) - Maxime, Claude Opus 5 (1M context), Claude Opus 5, Claude Sonnet 5, Claude
- Mise en place repo - ([7306d51](https://github.com/batleforc/weebo-notify-v2/commit/7306d51a41c2d7b33c27d65db962367fb8a52a20)) - Max Batleforc
#### Bug Fixes
- (**app**) log a push body that will not serialise before falling back - ([4716f75](https://github.com/batleforc/weebo-notify-v2/commit/4716f75f63938f849baf6e13cc5ece66961bf5d9)) - Max Batleforc, Claude Opus 5.5
- (**app**) a profile edit no longer erases a concurrent one - ([d18253d](https://github.com/batleforc/weebo-notify-v2/commit/d18253d71d2e75fad29440587be6bb970fc384e8)) - Max Batleforc, Claude Opus 5.5
- (**audit**) keep local k6 results out of the secret scan - ([4b87e5a](https://github.com/batleforc/weebo-notify-v2/commit/4b87e5aed05b7e3924b69d3c45b0b7b1e9efa859)) - Max Batleforc, Claude Opus 5.5
- (**deps**) task audit green — bump the desktop lockfile, two scoped exceptions - ([8fec716](https://github.com/batleforc/weebo-notify-v2/commit/8fec7160f5927406488d6ef5e76591c0547d494c)) - Max Batleforc, Claude Opus 5.5
- (**deps**) move secret-key crypto to aws-lc, closing RUSTSEC-2026-0387 - ([3d09f12](https://github.com/batleforc/weebo-notify-v2/commit/3d09f127b8d8b3d0667459c58ec0f61a4a0106d6)) - Max Batleforc, Claude Opus 5.5
- (**deps**) bump majors, add dependency hygiene, drop unused deps - ([ee1fcc2](https://github.com/batleforc/weebo-notify-v2/commit/ee1fcc20ed75c31fd547b9423ad55bc5fdd87ece)) - Max Batleforc, Claude Opus 5.5
- (**desktop**) hold the Tauri crate to the workspace lints - ([02d1651](https://github.com/batleforc/weebo-notify-v2/commit/02d16518c86030a0e76bad59e7d96eaa46ce2ab5)) - Max Batleforc, Claude Opus 5.5
- (**publish-main**) scan only the platform it builds - ([4f184c2](https://github.com/batleforc/weebo-notify-v2/commit/4f184c2a7a3563801d30c86451e5fe90e4385a12)) - Max Batleforc, Claude Opus 5.5
- (**release**) bump the JSON manifests with sed, never yq - ([81f4e2e](https://github.com/batleforc/weebo-notify-v2/commit/81f4e2ed9371daadf9d26fd1e03dcb42b66466df)) - Max Batleforc, Claude Opus 5.5
- (**tauri**) drop the duplicated rust-ini target table - ([18aea8c](https://github.com/batleforc/weebo-notify-v2/commit/18aea8cf5e4e621d7b65855c6bff26c2b02fe158)) - Max Batleforc, Claude Opus 5.5
- (**test**) stop the browser:ci servers on exit - ([9cb9339](https://github.com/batleforc/weebo-notify-v2/commit/9cb9339be64a99e9171e434ac3bdf6e9415e662b)) - Max Batleforc, Claude Opus 5.5
- (**web**) check indexed access and drop the unchecked casts - ([a693542](https://github.com/batleforc/weebo-notify-v2/commit/a69354216621f9207cad35ee68c2454aa197745d)) - Max Batleforc, Claude Opus 5.5
- close the second 2026-10-24 review - ([6c624d9](https://github.com/batleforc/weebo-notify-v2/commit/6c624d9d2214560ecedcf38ae0b98663a1ec8ffa)) - Max Batleforc, Claude Opus 5.5
- close the 2026-10-24 audit findings - ([f2032ed](https://github.com/batleforc/weebo-notify-v2/commit/f2032ed1406645ee4ee44bd2657298b9a1c7aeee)) - Max Batleforc, Claude Opus 5.5
#### Performance Improvements
- (**persistence-pg**) index the two counts every /sync page runs - ([cfa6d77](https://github.com/batleforc/weebo-notify-v2/commit/cfa6d77888aa049c1d41a8c0f836fb485dbf5894)) - Max Batleforc, Claude Opus 5.5
- (**persistence-pg**) page the inbox per channel instead of sorting every channel - ([5a0b21e](https://github.com/batleforc/weebo-notify-v2/commit/5a0b21e8405ab774fe04a9efcbaf3a349c82f6f4)) - Max Batleforc, Claude Opus 5.5
- (**stream**) render each live event once per replica, not once per stream - ([8c46c37](https://github.com/batleforc/weebo-notify-v2/commit/8c46c3766e433f6e5bd5728567fb69d44d3a2418)) - Max Batleforc, Claude Opus 5.5
- (**stream**) one live reader per replica instead of one read per stream - ([e97cc5e](https://github.com/batleforc/weebo-notify-v2/commit/e97cc5ed4cc719e97df68fbc219a37dff6779e6b)) - Max Batleforc, Claude Opus 5.5
#### Tests
- (**fuzz**) cargo-fuzz targets for the domain's parsers of untrusted strings - ([081f434](https://github.com/batleforc/weebo-notify-v2/commit/081f434f633ffdf84f1ab9078407998652bfd42e)) - Max Batleforc, Claude Opus 5.5
- (**perf**) k6 load scenarios, README, and the two bottlenecks they found - ([7e3fe09](https://github.com/batleforc/weebo-notify-v2/commit/7e3fe09ce1a46efc99f49d9b8a6358cacaa89d04)) - Max Batleforc, Claude Opus 5.5
- (**web**) raise the coverage floor to what the suite measures - ([6fa623e](https://github.com/batleforc/weebo-notify-v2/commit/6fa623edd78e665b5e9aa6a1d3e3ebec320e79dd)) - Max Batleforc, Claude Opus 5.5
#### Build system
- (**image**) move the base digests with updatecli, slim the build context - ([57cc529](https://github.com/batleforc/weebo-notify-v2/commit/57cc529c92ad4e433aba36fe0b7bfc5d597b3a08)) - Max Batleforc, Claude Opus 5.5
- publish as ghcr.io/batleforc/weebo-notify-v2 - ([4ddb1c1](https://github.com/batleforc/weebo-notify-v2/commit/4ddb1c117623b2e09b848b714b1372785d31e374)) - Max Batleforc, Claude Opus 5.5
#### Continuous Integration
- (**dependabot**) skip the two holds already written down - ([bd60200](https://github.com/batleforc/weebo-notify-v2/commit/bd60200a622faab36fe8195a090ac5d2f9219990)) - Max Batleforc, Claude Opus 5.5
- (**release**) push with the job's GITHUB_TOKEN instead of REGISTRY_TOKEN - ([0895bae](https://github.com/batleforc/weebo-notify-v2/commit/0895baefe0b5b4b2b5827db70305e2e7b411b3e8)) - Max Batleforc, Claude Opus 5.5
- (**security**) secret scan, Semgrep, CodeQL, daily image scan, Dependabot - ([b6b7a6f](https://github.com/batleforc/weebo-notify-v2/commit/b6b7a6f68fd57657a63af204dad4517766bec200)) - Max Batleforc, Claude Opus 5.5
- bring every action and downloaded tool to its latest release - ([4e59f0d](https://github.com/batleforc/weebo-notify-v2/commit/4e59f0d0d3a410ca63463b623204b0a74bd06b15)) - Max Batleforc, Claude Opus 5.5
#### Refactoring
- (**app,domain**) overflow-free arithmetic - ([ed00288](https://github.com/batleforc/weebo-notify-v2/commit/ed00288fba0fbfefc1ed9dfcad0668e96da4d130)) - Max Batleforc, Claude Opus 5.5
- deny arithmetic side effects workspace-wide - ([7902c60](https://github.com/batleforc/weebo-notify-v2/commit/7902c60bbd683ba04b9f0664311dc2ccc9c9c2ad)) - Max Batleforc, Claude Opus 5.5
- every lint escape is an #[expect] with a reason - ([be18a22](https://github.com/batleforc/weebo-notify-v2/commit/be18a2280e246395a749cb6004f48bc93012542a)) - Max Batleforc, Claude Opus 5.5
- deny unreachable, todo, dbg and friends outside tests - ([5599f9e](https://github.com/batleforc/weebo-notify-v2/commit/5599f9e7a4e90fcde30a68f413aa0708f41eab8d)) - Max Batleforc, Claude Opus 5.5
- deny indexing and slicing outside tests - ([11efb19](https://github.com/batleforc/weebo-notify-v2/commit/11efb19966793de5cc29e8b0d6f3c44a8251c493)) - Max Batleforc, Claude Opus 5.5
- deny let-underscore on must-use values, workspace-wide - ([f5edf9c](https://github.com/batleforc/weebo-notify-v2/commit/f5edf9c2c3f041b21f1512a7c99a3c425eaf2163)) - Max Batleforc, Claude Opus 5.5
#### Miscellaneous Chores
- (**deps**) bump resvg to 0.49 - ([732e59d](https://github.com/batleforc/weebo-notify-v2/commit/732e59dd7b0dfb12653956785485cfa215b238dc)) - Max Batleforc, Claude Opus 5.5
- (**deps**) bump in range; pin uv, updatecli and cargo-fuzz - ([49aace8](https://github.com/batleforc/weebo-notify-v2/commit/49aace899b91991b2de4aa1cc8c206719f3e4b65)) - Max Batleforc, Claude Opus 5.5
- (**deps**) raise rust-version to 1.94 and pick up late releases - ([b61ca3a](https://github.com/batleforc/weebo-notify-v2/commit/b61ca3a2795350511d4acc286ee264894a73f39f)) - Max Batleforc, Claude Opus 5.5 (1M context)
- (**mise**) record the awscli 2.38.5 checksum - ([cc7e48d](https://github.com/batleforc/weebo-notify-v2/commit/cc7e48d9adbb71056c00c3a18b80aaecb538aa2e)) - Max Batleforc, Claude Opus 5.5
- (**release**) regenerate the contract after the version sync - ([358f5b5](https://github.com/batleforc/weebo-notify-v2/commit/358f5b5531c2f19e68049a190e136e89edc716a5)) - Max Batleforc, Claude Opus 5.5
- (**skills**) link batlehub-rfc and gitignore the wska links - ([08c8f19](https://github.com/batleforc/weebo-notify-v2/commit/08c8f19c562323389c2f82ac70920f231bea799c)) - Max Batleforc, Claude Opus 5.5
- (**skills**) record wska selection in .claude/wska.toml and bump wska to 0.12.0 - ([e2b85d8](https://github.com/batleforc/weebo-notify-v2/commit/e2b85d8968255d254ca8e61869c6ca36a15c6274)) - Max Batleforc, Claude Opus 5.5 (1M context)
- (**version**) v0.1.0-rc.0 - ([ec23887](https://github.com/batleforc/weebo-notify-v2/commit/ec23887dc52b9788cb3c785ac52e2f5bbf28bc0d)) - Max Batleforc
- keep the line the graphify installer appends to .gitignore - ([103b73e](https://github.com/batleforc/weebo-notify-v2/commit/103b73ea08dbfa8632a10f4eff101e69518b9e99)) - Max Batleforc, Claude Opus 5.5
- merge origin/main (parallel dependency upgrade) - ([a3cd251](https://github.com/batleforc/weebo-notify-v2/commit/a3cd2519d4a0a1c0f263cfdd5e999293d2430fda)) - Max Batleforc, Claude Opus 5.5
- ignore wska skill links and installer backups - ([2b205b8](https://github.com/batleforc/weebo-notify-v2/commit/2b205b8ca1c6061b0753ebcc2f0b40bc1ebf8573)) - Max Batleforc, Claude Opus 5.5
- align with weebo-base 4d190e8 and upgrade every dependency - ([225a4b3](https://github.com/batleforc/weebo-notify-v2/commit/225a4b326e94bfe45dcd4184ac9f29a57f1e1a82)) - Max Batleforc, Claude Opus 5.5 (1M context)

- - -

## [v0.1.0-rc.1](https://github.com/batleforc/weebo-notify-v2/compare/v0.1.0-rc.0..v0.1.0-rc.1) - 2026-10-25
#### Bug Fixes
- (**authentik**) allow the grants the login and the compat suite use - ([2bbd361](https://github.com/batleforc/weebo-notify-v2/commit/2bbd361c1883232ff135f949f10153ad69229776)) - Max Batleforc, Claude Opus 5.5
- (**deps**) move crypto-bigint off its yanked 0.7.x releases - ([2becbd5](https://github.com/batleforc/weebo-notify-v2/commit/2becbd51581394c8488771659099941a935c6799)) - Max Batleforc, Claude Opus 5.5
- (**release**) sync the version into the fuzz and Tauri lockfiles - ([9593895](https://github.com/batleforc/weebo-notify-v2/commit/9593895e377e85c7268990ac9ce3826db68e9c46)) - Max Batleforc, Claude Opus 5.5
#### Miscellaneous Chores
- (**release**) leave CHANGELOG.md out of the clean-tree pre-hook - ([afbf0e1](https://github.com/batleforc/weebo-notify-v2/commit/afbf0e11c4497c875621dd55cf07a076845b0dfa)) - Max Batleforc, Claude Opus 5.5

- - -

## [v0.1.0-rc.0](https://github.com/batleforc/weebo-notify-v2/compare/976ebfe5dafeb3416ad00cf6335c1ed0bc11ff89..v0.1.0-rc.0) - 2026-10-25
#### Features
- sec review - ([385df89](https://github.com/batleforc/weebo-notify-v2/commit/385df895a956a40874df0af2c0107d5b00d00f98)) - Max Batleforc
- production readiness, BatleHub-style UI, ecosystem compat and device management (#3) - ([4a7e86a](https://github.com/batleforc/weebo-notify-v2/commit/4a7e86a347515c2da44660b0d591cf41c58c9403)) - Maxime, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude Opus 5.5, Claude
- SECURE DA BOUZIN - ([e18c718](https://github.com/batleforc/weebo-notify-v2/commit/e18c718c6a136a5062cb9acf3fd256492928655f)) - Max Batleforc
- WIP (#2) - ([6463e12](https://github.com/batleforc/weebo-notify-v2/commit/6463e1229e57e52e20d5d9472ca40071fad384cf)) - Maxime
- socle projet weebo-notify (M0) (#1) - ([f1ec9ff](https://github.com/batleforc/weebo-notify-v2/commit/f1ec9ff6fb8a0e96c62c4567e41cf8b6da679ab3)) - Maxime, Claude Opus 5 (1M context), Claude Opus 5, Claude Sonnet 5, Claude
- Mise en place repo - ([7306d51](https://github.com/batleforc/weebo-notify-v2/commit/7306d51a41c2d7b33c27d65db962367fb8a52a20)) - Max Batleforc
#### Bug Fixes
- (**app**) log a push body that will not serialise before falling back - ([4716f75](https://github.com/batleforc/weebo-notify-v2/commit/4716f75f63938f849baf6e13cc5ece66961bf5d9)) - Max Batleforc, Claude Opus 5.5
- (**app**) a profile edit no longer erases a concurrent one - ([d18253d](https://github.com/batleforc/weebo-notify-v2/commit/d18253d71d2e75fad29440587be6bb970fc384e8)) - Max Batleforc, Claude Opus 5.5
- (**audit**) keep local k6 results out of the secret scan - ([4b87e5a](https://github.com/batleforc/weebo-notify-v2/commit/4b87e5aed05b7e3924b69d3c45b0b7b1e9efa859)) - Max Batleforc, Claude Opus 5.5
- (**deps**) task audit green — bump the desktop lockfile, two scoped exceptions - ([8fec716](https://github.com/batleforc/weebo-notify-v2/commit/8fec7160f5927406488d6ef5e76591c0547d494c)) - Max Batleforc, Claude Opus 5.5
- (**deps**) move secret-key crypto to aws-lc, closing RUSTSEC-2026-0387 - ([3d09f12](https://github.com/batleforc/weebo-notify-v2/commit/3d09f127b8d8b3d0667459c58ec0f61a4a0106d6)) - Max Batleforc, Claude Opus 5.5
- (**deps**) bump majors, add dependency hygiene, drop unused deps - ([ee1fcc2](https://github.com/batleforc/weebo-notify-v2/commit/ee1fcc20ed75c31fd547b9423ad55bc5fdd87ece)) - Max Batleforc, Claude Opus 5.5
- (**desktop**) hold the Tauri crate to the workspace lints - ([02d1651](https://github.com/batleforc/weebo-notify-v2/commit/02d16518c86030a0e76bad59e7d96eaa46ce2ab5)) - Max Batleforc, Claude Opus 5.5
- (**publish-main**) scan only the platform it builds - ([4f184c2](https://github.com/batleforc/weebo-notify-v2/commit/4f184c2a7a3563801d30c86451e5fe90e4385a12)) - Max Batleforc, Claude Opus 5.5
- (**release**) bump the JSON manifests with sed, never yq - ([81f4e2e](https://github.com/batleforc/weebo-notify-v2/commit/81f4e2ed9371daadf9d26fd1e03dcb42b66466df)) - Max Batleforc, Claude Opus 5.5
- (**tauri**) drop the duplicated rust-ini target table - ([18aea8c](https://github.com/batleforc/weebo-notify-v2/commit/18aea8cf5e4e621d7b65855c6bff26c2b02fe158)) - Max Batleforc, Claude Opus 5.5
- (**test**) stop the browser:ci servers on exit - ([9cb9339](https://github.com/batleforc/weebo-notify-v2/commit/9cb9339be64a99e9171e434ac3bdf6e9415e662b)) - Max Batleforc, Claude Opus 5.5
- (**web**) check indexed access and drop the unchecked casts - ([a693542](https://github.com/batleforc/weebo-notify-v2/commit/a69354216621f9207cad35ee68c2454aa197745d)) - Max Batleforc, Claude Opus 5.5
- close the second 2026-10-24 review - ([6c624d9](https://github.com/batleforc/weebo-notify-v2/commit/6c624d9d2214560ecedcf38ae0b98663a1ec8ffa)) - Max Batleforc, Claude Opus 5.5
- close the 2026-10-24 audit findings - ([f2032ed](https://github.com/batleforc/weebo-notify-v2/commit/f2032ed1406645ee4ee44bd2657298b9a1c7aeee)) - Max Batleforc, Claude Opus 5.5
#### Performance Improvements
- (**persistence-pg**) index the two counts every /sync page runs - ([cfa6d77](https://github.com/batleforc/weebo-notify-v2/commit/cfa6d77888aa049c1d41a8c0f836fb485dbf5894)) - Max Batleforc, Claude Opus 5.5
- (**persistence-pg**) page the inbox per channel instead of sorting every channel - ([5a0b21e](https://github.com/batleforc/weebo-notify-v2/commit/5a0b21e8405ab774fe04a9efcbaf3a349c82f6f4)) - Max Batleforc, Claude Opus 5.5
- (**stream**) render each live event once per replica, not once per stream - ([8c46c37](https://github.com/batleforc/weebo-notify-v2/commit/8c46c3766e433f6e5bd5728567fb69d44d3a2418)) - Max Batleforc, Claude Opus 5.5
- (**stream**) one live reader per replica instead of one read per stream - ([e97cc5e](https://github.com/batleforc/weebo-notify-v2/commit/e97cc5ed4cc719e97df68fbc219a37dff6779e6b)) - Max Batleforc, Claude Opus 5.5
#### Tests
- (**fuzz**) cargo-fuzz targets for the domain's parsers of untrusted strings - ([081f434](https://github.com/batleforc/weebo-notify-v2/commit/081f434f633ffdf84f1ab9078407998652bfd42e)) - Max Batleforc, Claude Opus 5.5
- (**perf**) k6 load scenarios, README, and the two bottlenecks they found - ([7e3fe09](https://github.com/batleforc/weebo-notify-v2/commit/7e3fe09ce1a46efc99f49d9b8a6358cacaa89d04)) - Max Batleforc, Claude Opus 5.5
- (**web**) raise the coverage floor to what the suite measures - ([6fa623e](https://github.com/batleforc/weebo-notify-v2/commit/6fa623edd78e665b5e9aa6a1d3e3ebec320e79dd)) - Max Batleforc, Claude Opus 5.5
#### Build system
- (**image**) move the base digests with updatecli, slim the build context - ([57cc529](https://github.com/batleforc/weebo-notify-v2/commit/57cc529c92ad4e433aba36fe0b7bfc5d597b3a08)) - Max Batleforc, Claude Opus 5.5
- publish as ghcr.io/batleforc/weebo-notify-v2 - ([4ddb1c1](https://github.com/batleforc/weebo-notify-v2/commit/4ddb1c117623b2e09b848b714b1372785d31e374)) - Max Batleforc, Claude Opus 5.5
#### Continuous Integration
- (**dependabot**) skip the two holds already written down - ([bd60200](https://github.com/batleforc/weebo-notify-v2/commit/bd60200a622faab36fe8195a090ac5d2f9219990)) - Max Batleforc, Claude Opus 5.5
- (**release**) push with the job's GITHUB_TOKEN instead of REGISTRY_TOKEN - ([0895bae](https://github.com/batleforc/weebo-notify-v2/commit/0895baefe0b5b4b2b5827db70305e2e7b411b3e8)) - Max Batleforc, Claude Opus 5.5
- (**security**) secret scan, Semgrep, CodeQL, daily image scan, Dependabot - ([b6b7a6f](https://github.com/batleforc/weebo-notify-v2/commit/b6b7a6f68fd57657a63af204dad4517766bec200)) - Max Batleforc, Claude Opus 5.5
- bring every action and downloaded tool to its latest release - ([4e59f0d](https://github.com/batleforc/weebo-notify-v2/commit/4e59f0d0d3a410ca63463b623204b0a74bd06b15)) - Max Batleforc, Claude Opus 5.5
#### Refactoring
- (**app,domain**) overflow-free arithmetic - ([ed00288](https://github.com/batleforc/weebo-notify-v2/commit/ed00288fba0fbfefc1ed9dfcad0668e96da4d130)) - Max Batleforc, Claude Opus 5.5
- deny arithmetic side effects workspace-wide - ([7902c60](https://github.com/batleforc/weebo-notify-v2/commit/7902c60bbd683ba04b9f0664311dc2ccc9c9c2ad)) - Max Batleforc, Claude Opus 5.5
- every lint escape is an #[expect] with a reason - ([be18a22](https://github.com/batleforc/weebo-notify-v2/commit/be18a2280e246395a749cb6004f48bc93012542a)) - Max Batleforc, Claude Opus 5.5
- deny unreachable, todo, dbg and friends outside tests - ([5599f9e](https://github.com/batleforc/weebo-notify-v2/commit/5599f9e7a4e90fcde30a68f413aa0708f41eab8d)) - Max Batleforc, Claude Opus 5.5
- deny indexing and slicing outside tests - ([11efb19](https://github.com/batleforc/weebo-notify-v2/commit/11efb19966793de5cc29e8b0d6f3c44a8251c493)) - Max Batleforc, Claude Opus 5.5
- deny let-underscore on must-use values, workspace-wide - ([f5edf9c](https://github.com/batleforc/weebo-notify-v2/commit/f5edf9c2c3f041b21f1512a7c99a3c425eaf2163)) - Max Batleforc, Claude Opus 5.5
#### Miscellaneous Chores
- (**deps**) bump resvg to 0.49 - ([732e59d](https://github.com/batleforc/weebo-notify-v2/commit/732e59dd7b0dfb12653956785485cfa215b238dc)) - Max Batleforc, Claude Opus 5.5
- (**deps**) bump in range; pin uv, updatecli and cargo-fuzz - ([49aace8](https://github.com/batleforc/weebo-notify-v2/commit/49aace899b91991b2de4aa1cc8c206719f3e4b65)) - Max Batleforc, Claude Opus 5.5
- (**deps**) raise rust-version to 1.94 and pick up late releases - ([b61ca3a](https://github.com/batleforc/weebo-notify-v2/commit/b61ca3a2795350511d4acc286ee264894a73f39f)) - Max Batleforc, Claude Opus 5.5 (1M context)
- (**mise**) record the awscli 2.38.5 checksum - ([cc7e48d](https://github.com/batleforc/weebo-notify-v2/commit/cc7e48d9adbb71056c00c3a18b80aaecb538aa2e)) - Max Batleforc, Claude Opus 5.5
- (**release**) regenerate the contract after the version sync - ([358f5b5](https://github.com/batleforc/weebo-notify-v2/commit/358f5b5531c2f19e68049a190e136e89edc716a5)) - Max Batleforc, Claude Opus 5.5
- (**skills**) link batlehub-rfc and gitignore the wska links - ([08c8f19](https://github.com/batleforc/weebo-notify-v2/commit/08c8f19c562323389c2f82ac70920f231bea799c)) - Max Batleforc, Claude Opus 5.5
- (**skills**) record wska selection in .claude/wska.toml and bump wska to 0.12.0 - ([e2b85d8](https://github.com/batleforc/weebo-notify-v2/commit/e2b85d8968255d254ca8e61869c6ca36a15c6274)) - Max Batleforc, Claude Opus 5.5 (1M context)
- keep the line the graphify installer appends to .gitignore - ([103b73e](https://github.com/batleforc/weebo-notify-v2/commit/103b73ea08dbfa8632a10f4eff101e69518b9e99)) - Max Batleforc, Claude Opus 5.5
- merge origin/main (parallel dependency upgrade) - ([a3cd251](https://github.com/batleforc/weebo-notify-v2/commit/a3cd2519d4a0a1c0f263cfdd5e999293d2430fda)) - Max Batleforc, Claude Opus 5.5
- ignore wska skill links and installer backups - ([2b205b8](https://github.com/batleforc/weebo-notify-v2/commit/2b205b8ca1c6061b0753ebcc2f0b40bc1ebf8573)) - Max Batleforc, Claude Opus 5.5
- align with weebo-base 4d190e8 and upgrade every dependency - ([225a4b3](https://github.com/batleforc/weebo-notify-v2/commit/225a4b326e94bfe45dcd4184ac9f29a57f1e1a82)) - Max Batleforc, Claude Opus 5.5 (1M context)

- - -

Changelog generated by [cocogitto](https://github.com/cocogitto/cocogitto).
