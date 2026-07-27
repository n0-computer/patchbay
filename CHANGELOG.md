# Changelog

## [0.7.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.6.0..0.7.0) - 2026-07-27

### ⛰️  Features

- Add test setup helpers to Lab builder ([#44](https://github.com/n0-computer/patchbay/issues/44)) - ([9f0d703](https://github.com/n0-computer/patchbay/commit/9f0d703fc821ba1957928b01cf7221470bba5450))
- [**breaking**] Realistic link emulation (congestion-buffer and bursty-loss knobs) ([#43](https://github.com/n0-computer/patchbay/issues/43)) - ([7014aaa](https://github.com/n0-computer/patchbay/commit/7014aaa542b4aa61a8c212dcac85116a28d22767))
- [**breaking**] Rework NAT taxonomy with behavior-only variants + ADF filtering ([#37](https://github.com/n0-computer/patchbay/issues/37)) - ([8415da5](https://github.com/n0-computer/patchbay/commit/8415da55db963fc03a9132475d9d6c1052c5d85a))

## [0.6.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.5.2..patchbay-v0.6.0) - 2026-06-15

### ⛰️  Features

- [**breaking**] Update deps and iroh-metrics@1.0 ([#42](https://github.com/n0-computer/patchbay/issues/42)) - ([4332285](https://github.com/n0-computer/patchbay/commit/4332285b8572df44bee3bdc46b538bd62e964c8d))

## [0.5.2](https://github.com/n0-computer/patchbay/compare/patchbay-v0.5.1..patchbay-v0.5.2) - 2026-05-07

### ⛰️  Features

- Update deps to latest ([#40](https://github.com/n0-computer/patchbay/issues/40)) - ([198bae0](https://github.com/n0-computer/patchbay/commit/198bae0afcdd21ad569b55c321d915933e9cc7a6))

## [0.5.1](https://github.com/n0-computer/patchbay/compare/patchbay-v0.5.0..patchbay-v0.5.1) - 2026-05-04

### ⛰️  Features

- Add cargo deny and fix issues ([#39](https://github.com/n0-computer/patchbay/issues/39)) - ([cecd3b2](https://github.com/n0-computer/patchbay/commit/cecd3b22e23396874169fa12d4441a6bdcaa1de9))

### ⚙️ Miscellaneous Tasks

- Refine tracing log levels for cleaner debug output ([#35](https://github.com/n0-computer/patchbay/issues/35)) - ([f2582ba](https://github.com/n0-computer/patchbay/commit/f2582bab6bddada635ddb405b2c329992e2cefec))

## [0.5.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.4.0..patchbay-v0.5.0) - 2026-04-14

### ⛰️  Features

- [**breaking**] LabDropGuard with FlushRegistry for guaranteed clean shutdown ([#29](https://github.com/n0-computer/patchbay/issues/29)) - ([4973d10](https://github.com/n0-computer/patchbay/commit/4973d1015db074dc8b9901b175530e2036a0a6f0))
- [**breaking**] Replace LabOpts with Lab::builder() pattern ([#31](https://github.com/n0-computer/patchbay/issues/31)) - ([09a33de](https://github.com/n0-computer/patchbay/commit/09a33de051bf67ed7ac2bb785647d6da299de6f7))

### 🐛 Bug Fixes

- Prevent mount namespace leaks and enforce user namespace isolation ([#30](https://github.com/n0-computer/patchbay/issues/30)) - ([275ac1d](https://github.com/n0-computer/patchbay/commit/275ac1db745c5b9347326d362934f5ccd46df74d))
- Use AbortOnDropHandle for DNS server task lifecycle ([#28](https://github.com/n0-computer/patchbay/issues/28)) - ([e001d5c](https://github.com/n0-computer/patchbay/commit/e001d5c23d14e27169c00192ce58272aa18a35a7))

### 📚 Documentation

- Update all documentation for DNS server and Iface API ([#25](https://github.com/n0-computer/patchbay/issues/25)) - ([78edca3](https://github.com/n0-computer/patchbay/commit/78edca33bc18502eb3124fcfe470a2eacb2a2714))

### ⚙️ Miscellaneous Tasks

- Remove stale files ([#23](https://github.com/n0-computer/patchbay/issues/23)) - ([859adc6](https://github.com/n0-computer/patchbay/commit/859adc6428603ba07eb5027f0d3e7f0db9152e2a))
- Replace iroh-integration with self-contained fixture ([#24](https://github.com/n0-computer/patchbay/issues/24)) - ([f1ef699](https://github.com/n0-computer/patchbay/commit/f1ef699f1c0c43063382af41de0509c18dea6d0c))
- Prepare 0.5.0 release ([#32](https://github.com/n0-computer/patchbay/issues/32)) - ([b3c9cf3](https://github.com/n0-computer/patchbay/commit/b3c9cf30c6535e19491cfdc78e0afac0112e6adf))

## [0.4.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.3.0..patchbay-v0.4.0) - 2026-04-10

### ⛰️  Features

- [**breaking**] Replace DNS overlay with in-process DNS server ([#13](https://github.com/n0-computer/patchbay/issues/13)) - ([699170f](https://github.com/n0-computer/patchbay/commit/699170f58efee64ffed010bf7e4c4cca8510e9d6))
- [**breaking**] Unified interface API with IfaceConfig, Iface handle, and isolated interfaces ([#21](https://github.com/n0-computer/patchbay/issues/21)) - ([ccc77d9](https://github.com/n0-computer/patchbay/commit/ccc77d944e01d194ec3ed1dacb0ec09185ba8e10))

### 🐛 Bug Fixes

- Two safeguard issues from crate review ([#15](https://github.com/n0-computer/patchbay/issues/15)) - ([5c3cab9](https://github.com/n0-computer/patchbay/commit/5c3cab93f007ad8707ac62cb9398e184760df80c))
- [**breaking**] Remove unused direction parameter from Lab::set_link_condition ([#18](https://github.com/n0-computer/patchbay/issues/18)) - ([4c2c0a2](https://github.com/n0-computer/patchbay/commit/4c2c0a2d5b8b0ea32deb7c67e81987d58c793155))

### 🚜 Refactor

- Split core.rs and handles.rs into focused modules ([#14](https://github.com/n0-computer/patchbay/issues/14)) - ([765e38b](https://github.com/n0-computer/patchbay/commit/765e38bb00b9dc46eb59d097b4e8dcf01ecb54db))
- Use rtnetlink for region link break/restore ([#16](https://github.com/n0-computer/patchbay/issues/16)) - ([8983acd](https://github.com/n0-computer/patchbay/commit/8983acd73938f251ab8cfe1fc43c9f5957701ace))

### ⚙️ Miscellaneous Tasks

- Remove plans/ and REVIEW.md from repo ([#19](https://github.com/n0-computer/patchbay/issues/19)) - ([359ef31](https://github.com/n0-computer/patchbay/commit/359ef31dcd78428c391534f16ddef07030e2f827))
- Prepare 0.4.0 release ([#22](https://github.com/n0-computer/patchbay/issues/22)) - ([c3b0f36](https://github.com/n0-computer/patchbay/commit/c3b0f36418bec043f02db646f382123b107dac14))

## [0.3.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.2.0..patchbay-v0.3.0) - 2026-03-31

### ⛰️  Features

- [**breaking**] Bidirectional per-device link impairment ([#12](https://github.com/n0-computer/patchbay/issues/12)) - ([a26cf72](https://github.com/n0-computer/patchbay/commit/a26cf729be9a918f420b8bb49642c30874031521))

### 🐛 Bug Fixes

- Prepend https:// to view_url when Host header lacks scheme ([#11](https://github.com/n0-computer/patchbay/issues/11)) - ([dff4873](https://github.com/n0-computer/patchbay/commit/dff4873db89647354d401c70104cebbf971f4c5b))

### ⚙️ Miscellaneous Tasks

- Prepare patchbay v0.3.0 release - ([6ad9f77](https://github.com/n0-computer/patchbay/commit/6ad9f77e9051483050ba5b310e67b5d7d5c14d2e))

## [0.2.0](https://github.com/n0-computer/patchbay/compare/patchbay-v0.1.0..patchbay-v0.2.0) - 2026-03-30

### ⛰️  Features

- Add patchbay-serve binary with push API, ACME TLS, and retention - ([aedf9c0](https://github.com/n0-computer/patchbay/commit/aedf9c05eda0574e846650e875b28f649529182d))
- Add systemd unit file, restructure testing docs - ([94f77b0](https://github.com/n0-computer/patchbay/commit/94f77b0a971ceb837ee2f37c3daf6993b89ace50))
- Add deep linking with react-router HashRouter - ([f92295b](https://github.com/n0-computer/patchbay/commit/f92295bb77074fb8ba1d37e760345f6d7e7a98d6))
- Flat run dirs, sims tab in invocation view, fix deep linking - ([c3d79aa](https://github.com/n0-computer/patchbay/commit/c3d79aac2cd21edf036d5d7465c4c6c0140b1c8f))
- Split bind into --http-bind and --https-bind - ([bc821ac](https://github.com/n0-computer/patchbay/commit/bc821ac5394154f67deb2bf4e1a8cd2256b57bb0))
- Add TestGuard for per-test pass/fail status in state.json - ([d75987a](https://github.com/n0-computer/patchbay/commit/d75987aed81ab8525d7e0cbac4a3e01970d9fc50))
- Emit TestCompleted event, sort _lab first in timeline - ([38f2226](https://github.com/n0-computer/patchbay/commit/38f2226a7f52e25d689c48e4e222e946d8ff4ec7))
- Move pushed-runs listing to React UI, add workflow template - ([866aa8f](https://github.com/n0-computer/patchbay/commit/866aa8fd6603e3a33a1752d4355a6cb6c448e55d))
- Add [matrix] expansion for sim TOML files - ([729bc67](https://github.com/n0-computer/patchbay/commit/729bc67bbb9df8a95c0e690c4476eb32c3cd5203))
- Rewrite variable refs during counted device expansion - ([7544b19](https://github.com/n0-computer/patchbay/commit/7544b19bcead4faa57b51b503cb4d02833c0a87e))
- Add Apple container backend for patchbay-vm ([#7](https://github.com/n0-computer/patchbay/issues/7)) - ([674b80b](https://github.com/n0-computer/patchbay/commit/674b80b250ad4b63fdd586e5961849e8e6770f63))
- Unified CLI, compare mode, metrics collection ([#9](https://github.com/n0-computer/patchbay/issues/9)) - ([3e92f5f](https://github.com/n0-computer/patchbay/commit/3e92f5f698e9ed7613a8bd1d34a66fdce3ff29f2))
- Improve run discovery and group pages ([#10](https://github.com/n0-computer/patchbay/issues/10)) - ([4fb9f73](https://github.com/n0-computer/patchbay/commit/4fb9f7355027d7827011ff96963be87a54bc48b1))

### 🐛 Bug Fixes

- Deduplicate timeline events, fix jump-to-log precision, style span rendering - ([2920b8c](https://github.com/n0-computer/patchbay/commit/2920b8c13ead82adaab5624c12a0b8c234ebd2e5))
- Correct view URL in CI snippet and runs index page - ([1cfe67a](https://github.com/n0-computer/patchbay/commit/1cfe67a4ef58edf5ff924207593d4404b26a1e60))
- Install rustls crypto provider at startup - ([6424cbb](https://github.com/n0-computer/patchbay/commit/6424cbbfe18d33b0a7e82ae2801e7df489e3e611))
- Bind ACME listeners on 0.0.0.0 instead of [::] - ([8f28d1f](https://github.com/n0-computer/patchbay/commit/8f28d1f4d8cdeebfcb1685a61df11fb755167ba6))
- Validate ACME port requirements at startup - ([ff8f0b6](https://github.com/n0-computer/patchbay/commit/ff8f0b67da49742de714ec59794bc6c52fe5334e))
- Replace SSE with REST for initial load, SSE only for live runs - ([e69d513](https://github.com/n0-computer/patchbay/commit/e69d5134e533397a341df57c13713b5b8ca5bea8))
- Block on writer completion in drop, create state.json if missing - ([88f2344](https://github.com/n0-computer/patchbay/commit/88f2344b7f148e517260e30ca2414a0045beda3d))
- Apply proper fullcone NAT rules for CGNAT (EIM+EIF) - ([78540fb](https://github.com/n0-computer/patchbay/commit/78540fb279ba897fc588b08b221b4147f09b6f33))
- Run prepare builds before binary assembly - ([c3139af](https://github.com/n0-computer/patchbay/commit/c3139aff509fd4bd55445fa77e1b35a5d3e49943))
- Extract AxisParams type alias to satisfy clippy type_complexity - ([5ed18dc](https://github.com/n0-computer/patchbay/commit/5ed18dc1f998debb97b4ad88fb9ff9da6afb0fb0))
- Bind-mount /proc/thread-self/net over /proc/net in namespaces ([#5](https://github.com/n0-computer/patchbay/issues/5)) - ([9bdd7fc](https://github.com/n0-computer/patchbay/commit/9bdd7fc528117b0bb1ce949399b95d1bc75c0dda))
- Reduce default capture timeout from 300s to 30s - ([887f9ff](https://github.com/n0-computer/patchbay/commit/887f9ffd3bb077c5c1ae732503808c1a1e98c03f))
- Rewrite results capture keys during counted device expansion ([#6](https://github.com/n0-computer/patchbay/issues/6)) - ([94876c4](https://github.com/n0-computer/patchbay/commit/94876c49879dc51c50dcf4503739c83013dad0f4))
- Share tracing dispatch between async and sync namespace workers - ([9f41d41](https://github.com/n0-computer/patchbay/commit/9f41d41e3030e38eba405e7052c4566f8f7831f0))

### 🚜 Refactor

- Sync final state.json write via shared LabState mutex - ([e905ff6](https://github.com/n0-computer/patchbay/commit/e905ff68254c70d25523715104d804cdf77c218d))
- Merge runs index, unify /api/runs with manifest data - ([8efa131](https://github.com/n0-computer/patchbay/commit/8efa1317b7a717b8311f01d5b431aadb15577e04))
- Put patchbay-server integration behind `serve` feature - ([57641d4](https://github.com/n0-computer/patchbay/commit/57641d454a1c5bdab714d47ba0fe1f0f9f63e64a))

### 📚 Documentation

- Add patchbay-server README with deploy and CI docs - ([705c0d4](https://github.com/n0-computer/patchbay/commit/705c0d4fb2d264821e9655886a1b0ead4e767d93))
- Add matrix expansion and when field to TOML reference - ([3a60cd9](https://github.com/n0-computer/patchbay/commit/3a60cd9d0539d01b0e03049e23c0a598c29b9e38))

### 🧪 Testing

- Add e2e test for push API and deep linking - ([b31372d](https://github.com/n0-computer/patchbay/commit/b31372d1cc2fa129f5103559a22ff3cc0f097ddb))
- Add tests for counted device ref rewriting - ([8fca4d0](https://github.com/n0-computer/patchbay/commit/8fca4d02522c59f35ec1bcc5f82cc5c74b7d7055))

### ⚙️ Miscellaneous Tasks

- Fmt - ([10e39e6](https://github.com/n0-computer/patchbay/commit/10e39e60b59e7f5fe4887ca574e060a8d721d6f7))
- Fmt - ([11685cc](https://github.com/n0-computer/patchbay/commit/11685cc975fa9c68990e2d89c0f5bf05f9fd4f40))
- Run CI on all pull requests - ([1637f14](https://github.com/n0-computer/patchbay/commit/1637f144851b9f477c649d8784cd0988b6a3bbd2))
- Fixup CI - ([52890e1](https://github.com/n0-computer/patchbay/commit/52890e15fbf55bd462a7e1317df5a672e1c61059))
- Prepare patchbay v0.2.0 release - ([03ab97d](https://github.com/n0-computer/patchbay/commit/03ab97d86a8d79089fca28b038b779451efa4139))

## [0.1.0] - 2026-03-12

### ⛰️  Features

- *(patchbay-vm)* Add Apple Silicon (aarch64) support - ([763de72](https://github.com/n0-computer/patchbay/commit/763de72d19519efd1bed62be8ef760961f141f61))
- *(sim)* Surface last out.log error in manifests and UI - ([58a2dff](https://github.com/n0-computer/patchbay/commit/58a2dff5022378fbc49e4f7364f28d00d74f3c93))
- *(sim)* Split stdio logs and add verbose live streaming - ([e8cae16](https://github.com/n0-computer/patchbay/commit/e8cae1628d731fa1bac3ed9d0052f2a0058fd9f3))
- *(sim)* Additive infrastructure for generic capture-based results (Steps 1-6) - ([7c32262](https://github.com/n0-computer/patchbay/commit/7c3226257a9ab00ad31c3864e838c1fab93235e9))
- *(sims)* Port all iroh/iperf sims to new template/group/captures model (Step 7) - ([69fad18](https://github.com/n0-computer/patchbay/commit/69fad18ab8c525d9846b0b777d29d705771da2b9))
- *(tracing)* Add structured span hierarchy with lab/router/device context - ([58be1cb](https://github.com/n0-computer/patchbay/commit/58be1cb2e40874ecec07c317003c1a0f752e7a14))
- *(ui)* Stabilize timeline selection and log navigation - ([2f0bd4c](https://github.com/n0-computer/patchbay/commit/2f0bd4cfed7416199ec464c62e62d01c97feffbe))
- *(ui-server)* Use fixed shared default port for run/serve - ([1573732](https://github.com/n0-computer/patchbay/commit/1573732d61ed13cd54bb67d2a725d922b53481e3))
- Phase 1 — multi-interface Device, unified add_router, DeviceBuilder - ([7d5a505](https://github.com/n0-computer/patchbay/commit/7d5a505fc5e240496908b845d1971f311de1ad87))
- Consolidate netns execution, docs, and vm tooling - ([5325073](https://github.com/n0-computer/patchbay/commit/5325073753846c719ec74b87e31e7a4c097f103e))
- Add iroh integration runner with vm workflow - ([0ba5574](https://github.com/n0-computer/patchbay/commit/0ba5574d105005f6b12be49580fb3f440fc47082))
- Switch dynamic ops to rtnetlink - ([b6bf73e](https://github.com/n0-computer/patchbay/commit/b6bf73e6792fb63507e5d1ba9cd3547330f946e5))
- Add iperf parser-backed sim reporting - ([171812a](https://github.com/n0-computer/patchbay/commit/171812a7d64578884eb0a929b91cdc5933d1f936))
- Port legacy iroh sims and add chuck-compatible reports - ([194356c](https://github.com/n0-computer/patchbay/commit/194356c2310415bc143792c0d841be4f7d8c8580))
- Add standalone netsim-vm runner - ([a35a2d8](https://github.com/n0-computer/patchbay/commit/a35a2d8da2ce5107bf6d82759061753fca0b456d))
- Refactor sim run layout and transfer args - ([92e2122](https://github.com/n0-computer/patchbay/commit/92e2122cb6a35963e26fcd3b8edeeda74779d397))
- Add run manifest and per-sim summaries - ([3404897](https://github.com/n0-computer/patchbay/commit/34048974201c343d3f571f10c2f155a2aa08e095))
- Add browser UI for sim results, logs, timeline and qlog - ([65a2990](https://github.com/n0-computer/patchbay/commit/65a299021efc12e01890b45ed9c3a9836f8798e1))
- Unify serve/progress UX and remove netsim VM runtime code - ([a9ae322](https://github.com/n0-computer/patchbay/commit/a9ae322f8cc06a75d4707f9aac93927285b1b13a))
- Refactor UI to manifest/progress-driven sim workspace - ([29255e2](https://github.com/n0-computer/patchbay/commit/29255e276e097815b509bb53e40e2e5d69423aff))
- Use shared URL cache for binaries and reduce cleanup log noise - ([66e73e9](https://github.com/n0-computer/patchbay/commit/66e73e9c6a03f82a6b09975813ca2fabd43f27f6))
- Add run overview UI and drop chuck compat outputs - ([9338cfc](https://github.com/n0-computer/patchbay/commit/9338cfcf3c34d49aab11e1f37568a00c59f1c7fb))
- Add ranged log loading and metadata-first log viewer - ([90643ce](https://github.com/n0-computer/patchbay/commit/90643ce7a329cac879ef31e291fe7755930a7b08))
- Add inspect and run-in interactive debug workflow - ([ac82689](https://github.com/n0-computer/patchbay/commit/ac826891ab42be4d5a1804c7d0b2d5381cdaae33))
- Complete rootless mode and remove setup-caps - ([a018d37](https://github.com/n0-computer/patchbay/commit/a018d371286809a8de28afa234b56c9e88b6b81e))
- Refactor sim steps and utilities - ([3aa5215](https://github.com/n0-computer/patchbay/commit/3aa5215e82f63fbcb6a118a51896346f8726b56f))
- Build ui in build script - ([8b1a3b4](https://github.com/n0-computer/patchbay/commit/8b1a3b4f1f5244686adb3e5b147d80963c3f5bbf))
- Add prepare workflow and no-build sim runs - ([f51789c](https://github.com/n0-computer/patchbay/commit/f51789ce86fa060052e5c65de5c430324df71069))
- Batch prepare builds and refresh README usage - ([8ba2bb1](https://github.com/n0-computer/patchbay/commit/8ba2bb1c16305d2435cc0c4705aa6852a36e4ad1))
- Implement IPv6 dual-stack and v6-only support - ([565286c](https://github.com/n0-computer/patchbay/commit/565286c3c1d233fba794479baef46827fb83375b))
- Implement fullcone NAT for UDP hole punching - ([0bd63bb](https://github.com/n0-computer/patchbay/commit/0bd63bb4ba66ab942d5cb417e7dab655c87dd802))
- Allow configuring downstream subnet on RouterBuilder - ([7dc8473](https://github.com/n0-computer/patchbay/commit/7dc84731c640895b3dbc7dd0097bfa0af65dab97))
- Per-device DNS entries via /etc/hosts bind-mount overlay - ([b0667fb](https://github.com/n0-computer/patchbay/commit/b0667fb847891c44fb263474d032e2bbb2d6568d))
- Mount-once write-through DNS overlay with resolv.conf support - ([b204d0f](https://github.com/n0-computer/patchbay/commit/b204d0fed03291a803be83178ea715f57de86e01))
- NAT presets, API renames, Home NAT holepunching - ([08844fb](https://github.com/n0-computer/patchbay/commit/08844fbd7a6a4e8e42643579139c901215a6ccb0))
- NatConfig builder API, rewrite NAT rule generation from config - ([772b352](https://github.com/n0-computer/patchbay/commit/772b352cf95967a656ae36ceb5d7c87fa579e9a7))
- Enhanced impairment presets, Nat::Custom, parallelized tests - ([ae842e8](https://github.com/n0-computer/patchbay/commit/ae842e8509b299fbe0b26eb956d78c4304e6b531))
- MTU control, ICMP frag blocking, node removal, IP management - ([0008731](https://github.com/n0-computer/patchbay/commit/00087310410fb02218aa60497ca7ec6ca7116f50))
- Implement first-class region routing with veths and netem - ([1ea6723](https://github.com/n0-computer/patchbay/commit/1ea6723bdef507a96a0ca75cc74d0ffa50d0e608))
- Add firewall presets (Corporate, CaptivePortal, Custom) - ([f949cba](https://github.com/n0-computer/patchbay/commit/f949cba29685e9d21f475fd6eb00f5b0147a1d8f))
- Add Device::add_iface and Device::remove_iface for runtime interface management - ([389be19](https://github.com/n0-computer/patchbay/commit/389be1903800637a1a481a403291b83fecbd7278))
- IPv6 stateful firewall, public GUA pool, NAT64 plan - ([df12349](https://github.com/n0-computer/patchbay/commit/df12349c0cd7eb71f198a28118ecdb1f12b25ad2))
- RouterPreset, unified FirewallConfig with PortPolicy - ([3dbc3ff](https://github.com/n0-computer/patchbay/commit/3dbc3ffbfbc22a74826261a3f9a0c1e07fd1db54))
- NAT64 userspace SIIT translator for IPv6-only mobile networks - ([07050a3](https://github.com/n0-computer/patchbay/commit/07050a3f2cd14c591cab5df9d68f1f399dcdff35))
- Unified devtools UI, per-namespace tracing, multi-run server - ([f5e3944](https://github.com/n0-computer/patchbay/commit/f5e394425bc3cc85be072c69d46a9862fe69cb6c))
- Add ipv6 link-local plumbing and router iface API - ([be820f1](https://github.com/n0-computer/patchbay/commit/be820f1cbc3b26de1ddca2e0c5a8c823f233abc7))
- Improve RA-driven IPv6 scoped default-route handling - ([ee3fea7](https://github.com/n0-computer/patchbay/commit/ee3fea772910e012877df213a5433f5ee394e1cb))
- Add router RA controls and worker gating - ([e729af5](https://github.com/n0-computer/patchbay/commit/e729af5c5ce05008c5e4e3c537dafd0f293946ea))
- Emit RA advertisement events with lifetime metadata - ([2053d34](https://github.com/n0-computer/patchbay/commit/2053d3454f572ea95190cc80af6070169a8eb162))
- Handle RA lifetime zero for radriven default routes - ([7752a12](https://github.com/n0-computer/patchbay/commit/7752a122865bd33ab752f1e576cd23b835a72845))
- Emit rs tracing events in radriven control path - ([6d96fa2](https://github.com/n0-computer/patchbay/commit/6d96fa2e65fc89d81eef812782584e1a8e93315e))
- Persist radriven router solicitation events to node logs - ([1af73cd](https://github.com/n0-computer/patchbay/commit/1af73cd813d130f742312427fb593c6ae6c098c1))
- Add runtime router RA controls for radriven route refresh - ([22e8b4c](https://github.com/n0-computer/patchbay/commit/22e8b4cb1ded18df280dbda9d65ed0d2e3b98e74))
- Reconcile radriven v6 default routes on runtime RA changes - ([988d3d0](https://github.com/n0-computer/patchbay/commit/988d3d016b5f983d12e3e39b241f829ddfc1beb7))
- Make ra worker react to runtime config updates - ([878350b](https://github.com/n0-computer/patchbay/commit/878350bfc658d2b9d433f8e0288f66368c9d2ed4))
- Add ipv6 deployment profiles for lab provisioning - ([d354e13](https://github.com/n0-computer/patchbay/commit/d354e13d3b33903c591031126986385ef3aa0779))
- Map router presets to recommended ipv6 profiles - ([e712356](https://github.com/n0-computer/patchbay/commit/e712356024ba5a2c6077a00812fc570393885374))
- Allow per-device ipv6 provisioning overrides - ([ac132e8](https://github.com/n0-computer/patchbay/commit/ac132e82383431ae6492bc7d107f517580a72032))
- Add --testdir flag to serve commands - ([19358e6](https://github.com/n0-computer/patchbay/commit/19358e6627bef6934221a8422c379b1059725e19))
- Improve patchbay-vm test command and fix virtiofs permissions - ([2a35cca](https://github.com/n0-computer/patchbay/commit/2a35cca613b217e6d0bd46896f7349ba59eae750))
- Support full directive syntax in PATCHBAY_LOG / RUST_LOG file filter - ([9c8dde1](https://github.com/n0-computer/patchbay/commit/9c8dde10be11eb175dac170414920067bd33f655))
- Add .tracing.log ANSI output per namespace - ([2668e1f](https://github.com/n0-computer/patchbay/commit/2668e1f38c480cb11a1da9341edfbdf987c23924))
- Add invocation grouping and combined results view to UI - ([66b7fc5](https://github.com/n0-computer/patchbay/commit/66b7fc582f8625a707db8417b588e246a79263fe))
- Add --project-root, --timeout flags and fix capture/interpolation edge cases - ([efb0f73](https://github.com/n0-computer/patchbay/commit/efb0f734957e587d93a35d2d9f2599bc08edad9b))
- Flat stdout/stderr log files, command lifecycle events, and UI improvements - ([f21a94f](https://github.com/n0-computer/patchbay/commit/f21a94f8757da84a97abc4692c8ff36a257b648d))
- Detect tracing-jsonl logs by content in server - ([bd53b9a](https://github.com/n0-computer/patchbay/commit/bd53b9a86906694a59b15ab0ac15da2d7151ceaf))
- Add patchbay.toml config and recursive sim directory scanning - ([af3801e](https://github.com/n0-computer/patchbay/commit/af3801e9928ae52738c19f259d8be7a6dae27648))

### 🐛 Bug Fixes

- *(net)* Repair NAT/routing wiring after refactor - ([e82ba83](https://github.com/n0-computer/patchbay/commit/e82ba83545fb6c934be23c3425abf7323b3fbc99))
- *(sim)* Improve failure output and repair nat-switch sim - ([390c54a](https://github.com/n0-computer/patchbay/commit/390c54a74e231df8ca26f6c7be3c47c4c8fe90da))
- *(sim)* Load binaries from extends files; fix assert syntax in sims - ([6541057](https://github.com/n0-computer/patchbay/commit/654105740297eba5f943b1a60e47d9c428afbcb6))
- *(ui)* Enforce wall-time ordering in timeline - ([e035886](https://github.com/n0-computer/patchbay/commit/e035886864363ce495ad938a3b05e3a51520b3ba))
- Improve vm run/test ergonomics - ([b41bdd2](https://github.com/n0-computer/patchbay/commit/b41bdd213c1acdeb4f99bc143f4affef639713a4))
- Align inspect with run backend and keep session interactive - ([397cd91](https://github.com/n0-computer/patchbay/commit/397cd91471640aa78ae13e364fae73574cd68875))
- Honor host build env and simplify transfer perf table - ([4ac34c3](https://github.com/n0-computer/patchbay/commit/4ac34c3f456a8571100738f80c5d426ef55f630f))
- Use cargo metadata target dir for local sim builds - ([d3ac4f5](https://github.com/n0-computer/patchbay/commit/d3ac4f556a3f2f44246d8c7be3b4d25b324c062b))
- Resolve 5 failing tests in netsim-core test suite - ([d306082](https://github.com/n0-computer/patchbay/commit/d306082104b041296165892a2d61499b3e8cf6c2))
- Apply region latency rules when routers are built via builder API - ([1333936](https://github.com/n0-computer/patchbay/commit/1333936e5fd2d015e1bae9b74b43f7e750d28eee))
- Complete IPv6 dual-stack test fixes and cleanup - ([bf50efa](https://github.com/n0-computer/patchbay/commit/bf50efa173b8d64a3a9b2dfd143d5fa5524c865e))
- Prevent EDEADLK panic in worker Drop, fix switch_uplink veth teardown - ([f457914](https://github.com/n0-computer/patchbay/commit/f4579148e219c6874551ae111e15a7729c5404e2))
- Address allocator overflow, doc inaccuracies, error propagation - ([7b02bcd](https://github.com/n0-computer/patchbay/commit/7b02bcd77b51ad592f990815619e6df860ac549d))
- Make loss_udp_moderate less flaky - ([7e76174](https://github.com/n0-computer/patchbay/commit/7e76174caa5c93189858987626e2ad688ecd13f6))
- V6 inter-region routing and test corrections - ([7be1c91](https://github.com/n0-computer/patchbay/commit/7be1c91e53fd2ec46d9cd9b036cf120da0552437))
- Enable unprivileged user namespaces in CI - ([330f51a](https://github.com/n0-computer/patchbay/commit/330f51a468532f281bf2bb8bab2bcbbeca42fa66))
- Make udp_send_recv_count robust against buffer overflow and reflector races - ([fa8f15d](https://github.com/n0-computer/patchbay/commit/fa8f15d9f6af7c90a2c065249b191093d4de5a2d))
- NPTv6 prefix translation and v6 routing bugs - ([74a2ed0](https://github.com/n0-computer/patchbay/commit/74a2ed07ac29c5f1d731af4affddc63e170cd870))
- Address code review findings (symlink safety, server lifecycle, event reducer) - ([9eb24ed](https://github.com/n0-computer/patchbay/commit/9eb24eddbaee94f6cc9302eed247b73a5d2621d5))
- Preserve tracing span chains and verify in devtools e2e - ([31b63df](https://github.com/n0-computer/patchbay/commit/31b63dfa6d46f72502b58b97e7d4412424497c39))
- Improve log kind detection and qlog rendering - ([ba5962b](https://github.com/n0-computer/patchbay/commit/ba5962bac400d06b019e0b2257c2ac29c502cca7))
- Resolve ipv6 link-local review findings - ([28ab0ca](https://github.com/n0-computer/patchbay/commit/28ab0ca45086ccb988d4b98a7cafa8479d4529d9))
- Resolve clippy unnecessary_unwrap warnings - ([21f00ba](https://github.com/n0-computer/patchbay/commit/21f00ba6dcb6c00266c006a148f5b8c0cb6a7abd))
- Retry tc commands on transient EAGAIN - ([d9b40ed](https://github.com/n0-computer/patchbay/commit/d9b40ed45e8446d7687d604daa8bd6f545797a1e))
- Reduce CI test parallelism to prevent EAGAIN flakes - ([30bbd4b](https://github.com/n0-computer/patchbay/commit/30bbd4bf9bf4532f2a981b498ed8ecbe9e87ab7d))
- Add error context to bare socket/command spawns for EAGAIN diagnosis - ([97ebb5e](https://github.com/n0-computer/patchbay/commit/97ebb5e0d10e7e9345f8306d8319db1fc5ca2a8a))
- Add loss tests to serial-heavy group and increase warmup deadline - ([2e19425](https://github.com/n0-computer/patchbay/commit/2e19425350b8d9090c4cecdfbf4470412798f00d))
- Relax tight timeouts and RTT bounds, enable CI cache on failure - ([e05f007](https://github.com/n0-computer/patchbay/commit/e05f00703143880b0aa927fb3dde9f8280826c71))
- Make spawn_reflector async with readiness signalling - ([49846a2](https://github.com/n0-computer/patchbay/commit/49846a2592fafea3f324b3a48e2b56d8affd3d31))
- Tighten ipv6 default-route handling and align ipv6 ll docs - ([9906e8f](https://github.com/n0-computer/patchbay/commit/9906e8f1601fb8823fa184408de006fe04ba1d44))
- Make spawn_reflector async with readiness signalling - ([1ee7570](https://github.com/n0-computer/patchbay/commit/1ee75703461f65349759eaa3536149df89f1c712))
- Make spawn_reflector async with readiness signalling - ([1fdf0ff](https://github.com/n0-computer/patchbay/commit/1fdf0ffa088e97c1b26e30a7b2a279b3013dc50c))
- Split e2e into parallel CI job, fix stale netsim references - ([7edde73](https://github.com/n0-computer/patchbay/commit/7edde73ab38e69e76d99045e352b1ca23cca5304))
- Remove stale iroh e2e test - ([8c8f3d6](https://github.com/n0-computer/patchbay/commit/8c8f3d6e46227280dfb833d72f266f1301421889))
- Apply netem loss after ARP warmup to prevent flaky loss tests - ([ddc5f79](https://github.com/n0-computer/patchbay/commit/ddc5f79d98cabb9087db776e7840424345b50860))
- Deduplicate v6 route dispatch, RA guard, and RS emit logic - ([9134fac](https://github.com/n0-computer/patchbay/commit/9134facc8886e8733876bb8122018991c70642d1))
- Json tree toggle, timeline detail cleanup, and lab stop status - ([aeec33e](https://github.com/n0-computer/patchbay/commit/aeec33eee56f32d1cdc84345605b3802a63ea350))
- Write stopped status on channel close instead of LabStopping event - ([8c2c333](https://github.com/n0-computer/patchbay/commit/8c2c3334b839a52635b163655c16a239da3a4c94))
- Close SSE connections on page unload and fix JsonTree key click - ([9310b1d](https://github.com/n0-computer/patchbay/commit/9310b1dbdb107e2abd02941290e81b877a190e69))
- Fix e2e test failures in CI - ([203c6e9](https://github.com/n0-computer/patchbay/commit/203c6e966c7eff3f30a7a430ed58649afa3cbd4b))

### 🚜 Refactor

- *(sim)* Remove iroh-specific code and legacy step fields (Commit C) - ([03eb9c2](https://github.com/n0-computer/patchbay/commit/03eb9c2546589e55bea5faa3ac8c6dd4fedc4a4a))
- Add Qdisc abstraction for impairments - ([c1c2371](https://github.com/n0-computer/patchbay/commit/c1c2371a49aff94b37d4cdc03d3c5e09bed7a32d))
- Simplify UI run views and transfer summaries - ([c1a239d](https://github.com/n0-computer/patchbay/commit/c1a239d5134168216cbace99e38336ab89671488))
- Remove ported sims for now - ([a04386f](https://github.com/n0-computer/patchbay/commit/a04386f7ed3b7057329e25705bd06e9dcca3df63))
- Update sim files - ([cb3dad7](https://github.com/n0-computer/patchbay/commit/cb3dad72bc15f99a60a9aedb26da4554ed97e250))
- DRY, docs, simplifications + REVIEW.md - ([f05bed9](https://github.com/n0-computer/patchbay/commit/f05bed9587ae919a64e70011bd6c4d1f7b8fac07))
- Apply REVIEW.md items 1–5, 7, 9–10; close 3, 6, 8 - ([d4be5a3](https://github.com/n0-computer/patchbay/commit/d4be5a3368ab2ca27efabbf1f2c5100c0104e1cf))
- Split monolith into netsim-core, netsim-utils, netsim, netsim-vm - ([d3d8112](https://github.com/n0-computer/patchbay/commit/d3d81121037cfe3d1009a6c894897d1f63c51bd9))
- Report layer to steps model, MB/s throughput, human-readable UI - ([ec4055e](https://github.com/n0-computer/patchbay/commit/ec4055e27953959b5f9ea243b7310d491fc724e4))
- Netsim-core cleanup — split lib.rs, internalize naming, remove aliases - ([88ac0c4](https://github.com/n0-computer/patchbay/commit/88ac0c4ef9771a3fdcf8362524adbdfc8f34aac8))
- Async namespace worker redesign + deterministic resource cleanup - ([a4d51b3](https://github.com/n0-computer/patchbay/commit/a4d51b3a5761f98a01d14bdcb5e14536af2e9244))
- Persistent Netlink per namespace, RouterBuilder, async lab ops - ([52a0568](https://github.com/n0-computer/patchbay/commit/52a0568a49b453c3c0b77c21ee4edba3798c0e11))
- New Lab API with Device/Router handles and instant construction - ([4149a73](https://github.com/n0-computer/patchbay/commit/4149a7396de7879b0aee51a5c893ccb726299efa))
- API cleanup — rename, deduplicate, remove debug cruft - ([77fd3f2](https://github.com/n0-computer/patchbay/commit/77fd3f2cb1d7233ec57d07bcac09a944278484ef))
- Migrate tests to Device/Router handle API, internalize core - ([1e10faf](https://github.com/n0-computer/patchbay/commit/1e10fafc8f9bb6d9737b325a568836e38b6edaa9))
- Remove ResourceList cleanup, simplify Netlink, add task spans - ([cd05f88](https://github.com/n0-computer/patchbay/commit/cd05f8846c65d36b707ce38e32a35e2e1f2be283))
- Some cleanups - ([4b19672](https://github.com/n0-computer/patchbay/commit/4b1967253fd83bcc984d21d6dc7a16bf36761a8d))
- Remove globals, add Ix handle, ns-free test helpers - ([69b7c66](https://github.com/n0-computer/patchbay/commit/69b7c66a96de5cbf6faec7c524f1dda7186ddbb2))
- Merge LabInner into NetworkCore, async reflector, remove TaskHandle - ([a44a97f](https://github.com/n0-computer/patchbay/commit/a44a97f9c57ad373039c5c2bd39c42f1889be07c))
- Remove Lab::ix_gw, use ix().gw() instead - ([517d892](https://github.com/n0-computer/patchbay/commit/517d89254c0b7d4104ed6b4e5c1794654bf1102d))
- Remove own_netns tracking, tighten visibility to pub(crate) - ([70af3c8](https://github.com/n0-computer/patchbay/commit/70af3c8a75e3db285a1b7447bafe9b0c833c7b36))
- Merge ns creation + async worker thread, clean up netns module - ([3b77b81](https://github.com/n0-computer/patchbay/commit/3b77b812249af4da47f5b90459aeb6c58d11b45a))
- API cleanup — rename Impair, remove deprecated, fix races - ([40adab7](https://github.com/n0-computer/patchbay/commit/40adab75d8c93f03d3d1ab672ec1525ac8b572f1))
- Consolidate test helpers, remove duplicates, fix loss_udp_moderate - ([005f125](https://github.com/n0-computer/patchbay/commit/005f125e51c2c34c6fc09382fb2d3960ac8f05ed))
- Split lab.rs into nat, firewall, and handles modules - ([c055eaf](https://github.com/n0-computer/patchbay/commit/c055eaf71a339927e86df61c34b1bf7dfab96585))
- Overhaul mutex/lock architecture for async soundness - ([edf30a7](https://github.com/n0-computer/patchbay/commit/edf30a7d80e56970a9fe8ad38a893bc6dfc8dd6d))
- Eliminate panics from Device/Router handles on removed nodes - ([fe56fa8](https://github.com/n0-computer/patchbay/commit/fe56fa8f3a7b680ec1757029799dde08572ca2c1))
- Make Lab::new() fallible instead of panicking - ([bc89f35](https://github.com/n0-computer/patchbay/commit/bc89f3577854f9409e8e6650841066284a472a55))
- Split monolithic tests.rs into 16 focused submodules - ([aa0a5a0](https://github.com/n0-computer/patchbay/commit/aa0a5a027d725b2fbda233709a6beea0b2285f49))
- Migrate String → Arc<str>, extract lock bodies into NetworkCore - ([56bd22b](https://github.com/n0-computer/patchbay/commit/56bd22be366f712a1fd16d98a60e8cf7f205614b))
- Rename spawn_command default to async, _sync suffix for sync - ([4810393](https://github.com/n0-computer/patchbay/commit/481039365c28fd70e64871f4fa72c1e366186673))
- Remove sync server API, add devtools docs - ([6f808da](https://github.com/n0-computer/patchbay/commit/6f808da7b7defd3eb80d630662d950101109e4c1))
- Drop patchbay dep from patchbay-server, remove live mode - ([47049d4](https://github.com/n0-computer/patchbay/commit/47049d42c3661f7544cebf4cd36bc5ef0ce4fa80))
- Replace events tab with lab_events file kind and DRY JSON rendering - ([e87a72e](https://github.com/n0-computer/patchbay/commit/e87a72e5463b34e10d3a4aa330c1e7613efaad7e))
- Introduce OutDir enum and recursive server scan - ([b0dd032](https://github.com/n0-computer/patchbay/commit/b0dd032ab2c6bdfbe5f6dcb8dbe391dd88b47282))
- Consolidate router presets and improve documentation - ([49d2696](https://github.com/n0-computer/patchbay/commit/49d26965dd6af3870c2fd49e6a39f7a16a016b56))
- Move simple example from patchbay-runner to patchbay - ([c7fb67a](https://github.com/n0-computer/patchbay/commit/c7fb67af9cd17267dc3f9f2571534384fc00ea29))
- Improve lib.rs docs, remove ObservedAddr, tighten visibility - ([2770ff4](https://github.com/n0-computer/patchbay/commit/2770ff41cf95c3ba36d7b5c7755eae9e84c0e4d8))

### 📚 Documentation

- Add rootless user-namespace implementation plan - ([bf9df20](https://github.com/n0-computer/patchbay/commit/bf9df2002e58548746d16bf5373988bae12fbb28))
- Add maintainability review plan - ([b8b523c](https://github.com/n0-computer/patchbay/commit/b8b523c0ca3687ec39e68f9d52b2e88c656cbca5))
- Add iroh-isolation implementation plan - ([ecb9cbd](https://github.com/n0-computer/patchbay/commit/ecb9cbd1d70f4649b154c41a1d159120e56bbabd))
- Add sim TOML reference and rewrite README - ([43be619](https://github.com/n0-computer/patchbay/commit/43be6198f18a01fa8f233d910e719c2bfd1086de))
- Readme and example - ([eb0f54c](https://github.com/n0-computer/patchbay/commit/eb0f54c1267ab50fd7e390329746f4052d87ff33))
- Add real-world network patterns guide - ([b1b5649](https://github.com/n0-computer/patchbay/commit/b1b5649a15566d0c57891035a804d086df77f133))
- Expand public API rustdoc across patchbay crate - ([625993b](https://github.com/n0-computer/patchbay/commit/625993bba114dbe50b4bcd4f31f5aafcdac813f7))
- Review all prose and doc comments for accuracy and wording - ([5e39711](https://github.com/n0-computer/patchbay/commit/5e3971194cc45da58f84b47d1a4a745970484d0f))
- Fix README API inaccuracies, add multi-region routing section - ([ff830a3](https://github.com/n0-computer/patchbay/commit/ff830a3f5f5909e1ba8e5275224781623849ea0f))
- Fix stale API references, add RouterPreset/firewall sections - ([b3c06d1](https://github.com/n0-computer/patchbay/commit/b3c06d1789e23d09b24257c20ce9cfe040ee9699))
- Add NAT64/MobileV6 to docs, move holepunching to docs/ - ([4416b87](https://github.com/n0-computer/patchbay/commit/4416b879fa7d3e35d132e0d1d6a6f179e793a547))
- Add mdbook with guide section and reorganize reference docs - ([9709bc5](https://github.com/n0-computer/patchbay/commit/9709bc5524d3daf26a65ad2fb4d24bb55454e3d1))
- Rewrite guide for better prose, add motivation chapter - ([084a1d4](https://github.com/n0-computer/patchbay/commit/084a1d4bafcd35bcd0aeddd350f86e482ffaa3e0))
- Ctor test pattern, VM guide, reorder NAT64, rework holepunching - ([57c1925](https://github.com/n0-computer/patchbay/commit/57c1925d087457a2d666a5d8fdeffd42681f3508))
- Replace README include with concise book introduction - ([36dd6eb](https://github.com/n0-computer/patchbay/commit/36dd6ebdcf07092044b23c247e6fe0009797065d))
- Add devtools & UI implementation plan - ([c9cd0a2](https://github.com/n0-computer/patchbay/commit/c9cd0a27b297ba76162ac74071a2c941d4a9c822))
- Document ipv6 link-local behavior and stabilize udp loss tests - ([65dbcbd](https://github.com/n0-computer/patchbay/commit/65dbcbdf597055fdd165418e572ce6817dcfaa53))
- Finalize ipv6 link-local plan completion status - ([3053fc4](https://github.com/n0-computer/patchbay/commit/3053fc465af13c467a76fe5a8d113a3056752d5c))
- Clarify ipv6 fidelity and add limitations reference - ([cba194c](https://github.com/n0-computer/patchbay/commit/cba194c1b49fbbe1ea435703ca14868cad3221aa))
- Refine prose style and normalize reference wording - ([d9ff2a4](https://github.com/n0-computer/patchbay/commit/d9ff2a4c162d84941bdea70a971b986e526c5c6e))
- Improve prose and writing style - ([72a8235](https://github.com/n0-computer/patchbay/commit/72a823551b3d3a06016dfd477796490caae49393))
- Update REVIEW.md and add Ipv6Profile divergence comment - ([79e5ae2](https://github.com/n0-computer/patchbay/commit/79e5ae26c82cf4b55b69abb30297e2479fe7aaab))
- Add fmt-log command to testing guide - ([b882445](https://github.com/n0-computer/patchbay/commit/b882445fa3b412244d2eb4d0c5fd908cbd9f39aa))

### 🎨 Styling

- Apply rustfmt to lab.rs clippy fix - ([4e62508](https://github.com/n0-computer/patchbay/commit/4e62508be0555359ba74ce826ced38f9ffa98b5f))
- Cargo fmt patchbay-vm - ([b4bc8db](https://github.com/n0-computer/patchbay/commit/b4bc8db75d772f6668614cd77efc90a85a7b3369))

### 🧪 Testing

- *(ui)* Add playwright e2e for iroh-1to1-nat run - ([82c4455](https://github.com/n0-computer/patchbay/commit/82c4455048fac89793805e8ef4ce7d8c1a8499de))
- Capture net_report QAD timeout with dev relay - ([0293521](https://github.com/n0-computer/patchbay/commit/0293521bed6389025aae39e6bde5bc984d9e55de))
- Strengthen ui e2e assertions - ([3c1ee4b](https://github.com/n0-computer/patchbay/commit/3c1ee4b5107bcf02db084051941da33fbd4a6cc5))
- Add 8 new coverage tests across feature areas - ([f45440f](https://github.com/n0-computer/patchbay/commit/f45440f4d0bddb88b5ec5549745bdb9b213894d6))
- Add coverage for BlockInbound, RouterPreset, and public v6 pool - ([c476e03](https://github.com/n0-computer/patchbay/commit/c476e03c8b9281d373a03c5c78277ad2072b87da))
- Cover local v6 routing without default router in radriven mode - ([48ae37c](https://github.com/n0-computer/patchbay/commit/48ae37c48441e0763459671684de35f2b80048fb))
- Cover ra worker shutdown on router removal - ([ae33db3](https://github.com/n0-computer/patchbay/commit/ae33db3593f7fa31a31c13651a8366961f6adbe7))
- Verify ra reconcile skips static override devices - ([f73fed1](https://github.com/n0-computer/patchbay/commit/f73fed1e82966e91a0a8fd886588a349b261394b))
- Cover static override route switching with runtime iface changes - ([8a9b8bf](https://github.com/n0-computer/patchbay/commit/8a9b8bf847b67be30a03cfca7486806871c20ce8))
- Assert static ipv6 mode skips ra and rs tasks - ([018d9db](https://github.com/n0-computer/patchbay/commit/018d9dbb3cd8cb7cc37da2d88c37c99b72fbcd8a))
- Add runner sim e2e test - ([917e5af](https://github.com/n0-computer/patchbay/commit/917e5afb765408c328cb021fdd3488d6116cb06e))

### ⚙️ Miscellaneous Tasks

- *(logging)* Normalize cleanup logs - ([b5c70a3](https://github.com/n0-computer/patchbay/commit/b5c70a324342566cc13e749213199431bf4c1d5f))
- *(sim)* Finalize relay/dev-env runtime updates - ([fd93dbb](https://github.com/n0-computer/patchbay/commit/fd93dbb921cfd7a96abf0cf520d01d51af3613dd))
- *(sim)* Align verbose log prefixes - ([753da3a](https://github.com/n0-computer/patchbay/commit/753da3a88f950720197d60ab31765574fbb004ad))
- Update .gitignore - ([35a66f3](https://github.com/n0-computer/patchbay/commit/35a66f3bd1d9b9619a1b33baf7871f6bda75debb))
- Remove outdated workflow file - ([172cfee](https://github.com/n0-computer/patchbay/commit/172cfee5667e0211b05eb2976b8523080c12cf0b))
- Remove obsolete qemu-vm.sh based instructions, use netsim-vm instead - ([9973d15](https://github.com/n0-computer/patchbay/commit/9973d1558b54c8d7a738fb85a4d0ed89dc391968))
- Remove #[serial] from all tests; drop serial_test dependency - ([2716ece](https://github.com/n0-computer/patchbay/commit/2716ece93f3a8a7a84c88e4357f3a0109716a4cf))
- Add missing types and fmt ui - ([5400adb](https://github.com/n0-computer/patchbay/commit/5400adb4ca3dd6ee38fd1bdf52e0fe0ea33d0820))
- Move tests to file - ([d5eb87c](https://github.com/n0-computer/patchbay/commit/d5eb87cf9bdf0252642445e322283090ae126559))
- Fmt - ([f4a3bc1](https://github.com/n0-computer/patchbay/commit/f4a3bc1dbbb63d103fa1733dfd557f030d830d28))
- Remove duplicate test definitions - ([8d535dc](https://github.com/n0-computer/patchbay/commit/8d535dc3bc64cfb1d9c02c16500c3bdd3028e8db))
- Fmt - ([afc7572](https://github.com/n0-computer/patchbay/commit/afc7572ad5e2c838ca653f9781f7c1cb018646f1))
- Add license - ([f8aecf8](https://github.com/n0-computer/patchbay/commit/f8aecf8cc7c3b4becbdec0bb7b37aa65ba15cd53))
- Cargo.toml file cleanup - ([e181ecc](https://github.com/n0-computer/patchbay/commit/e181ecc3458d64e97337861e9ea98fdfee8aabe7))
- Mark no-globals-better-spawn plan as complete - ([28d52e6](https://github.com/n0-computer/patchbay/commit/28d52e64eb641836d30597704e69075183c651ca))
- Name all threads, configure nextest parallelism - ([5dbf808](https://github.com/n0-computer/patchbay/commit/5dbf80820af431e826b487c662de2712858e17f3))
- Review fixes — remove dead code, dup docstring, stabilize flaky test - ([339f355](https://github.com/n0-computer/patchbay/commit/339f35577886178af8d420c47c1ca9e4076e4de6))
- RouterBuilder::error helper, remove parse().unwrap(), dead code - ([235aa1d](https://github.com/n0-computer/patchbay/commit/235aa1dfe9c9feec19d1612c0dfc1752b0313191))
- Remove dead code and fix clippy warnings - ([6130feb](https://github.com/n0-computer/patchbay/commit/6130feb069d90cb908b5da100da7186e97003ad2))
- Remove deprecated API, replace stale tests, add lint gates - ([5212853](https://github.com/n0-computer/patchbay/commit/52128535dcdf5fc2ef79bc7933833f2c31ccc381))
- Fmt - ([2210e90](https://github.com/n0-computer/patchbay/commit/2210e9050ce09e2d81871e29d807bc5575388834))
- Add GitHub CI workflow and update format instructions - ([3bfc41c](https://github.com/n0-computer/patchbay/commit/3bfc41ca15179991086b474b6f801fc7fb7dc5c1))
- Add GitHub Pages workflow for mdbook - ([0365a50](https://github.com/n0-computer/patchbay/commit/0365a50282a96c52ec5c076064a7e5562b510360))
- Add rolling release CI workflow - ([a870303](https://github.com/n0-computer/patchbay/commit/a8703031eae5caaeeb814d7f2106a2c4d136d746))
- Fixup CI - ([f47f582](https://github.com/n0-computer/patchbay/commit/f47f5821346be0ae7a2cd47d566c42ddffcbaa4b))
- Update gitignore - ([9355fc4](https://github.com/n0-computer/patchbay/commit/9355fc4c8cfd5816d410ede0f7e8519411a41bf9))
- Fmt - ([1c1cd13](https://github.com/n0-computer/patchbay/commit/1c1cd13ce2fd1cd27acc6fe92becdd288fac3559))
- Fmt patchbay-runner - ([c300b99](https://github.com/n0-computer/patchbay/commit/c300b995f5d4a3410c78f09f64c98226032e315b))
- Bump GitHub Actions to v5 to fix Node.js 20 deprecation - ([8b53233](https://github.com/n0-computer/patchbay/commit/8b53233c3afcf570e94e49bcd40de3f8af6872a4))
- Update .gitignore - ([e0c48e0](https://github.com/n0-computer/patchbay/commit/e0c48e094ae11442b1ea374b6d4d8fd55a498ac1))

### Deps

- Cleanup and bump all deps - ([67fe8f0](https://github.com/n0-computer/patchbay/commit/67fe8f049e0b09789f7968c2e479ea03fe5f8982))

### Merge

- Bring main into ip6-link-local - ([80e1001](https://github.com/n0-computer/patchbay/commit/80e1001897483f71d793010a5ecd6f9b5d7aa10a))
- Bring main into ip6-link-local - ([0058fe3](https://github.com/n0-computer/patchbay/commit/0058fe3f832e06c5bcf9e991c3b15c30e426bf25))

### Plan

- Add exhaustive ipv6 link-local implementation roadmap - ([9a8989c](https://github.com/n0-computer/patchbay/commit/9a8989c58cbc9d456c0c7757b1a5415e65459d47))

### Plans

- Initial commit - ([8f0a55d](https://github.com/n0-computer/patchbay/commit/8f0a55def4784d41b631378b59691f45b1e2aef4))
- Add workspace-reorg.md for 4-crate split proposal - ([61f2ea4](https://github.com/n0-computer/patchbay/commit/61f2ea492d0798388f9abc1bbdd80ca22d966af5))
- Add dns,ipv6,testing plans - ([e5abc74](https://github.com/n0-computer/patchbay/commit/e5abc74406c5bd7a653a636f5cfc62daa0cefac0))
- Restructure plan management system - ([d6f9279](https://github.com/n0-computer/patchbay/commit/d6f9279ba127afc7d38b0776846ffbfa6ae62dd3))
- Update dns plan - ([82f3e28](https://github.com/n0-computer/patchbay/commit/82f3e283b69b3d80d636ee92cf9904fd90db4bd5))
- Add new-api plan - ([af9858d](https://github.com/n0-computer/patchbay/commit/af9858df87e7eb4a33ba2843989daabff8d33386))
- Mark new-api complete, update ipv6 plan for new API - ([0dc7bc8](https://github.com/n0-computer/patchbay/commit/0dc7bc848f2ab5aad0a455131852438f69551522))
- Mark ipv6 plan complete - ([b89f3f4](https://github.com/n0-computer/patchbay/commit/b89f3f4a07e9d7146152956cb1767426801918bd))
- Rewrite DNS plan focused on Rust API hosts-file overlay - ([966d38e](https://github.com/n0-computer/patchbay/commit/966d38eecfb1232923cd9c35fd6cdd17677eec7d))
- Mark DNS phase 1 as complete - ([7e230c9](https://github.com/n0-computer/patchbay/commit/7e230c944de7185bdb9af8c99a7aa2037b045b87))
- Add real-world-conditions, virtual-time, update PLAN.md - ([8d29f0f](https://github.com/n0-computer/patchbay/commit/8d29f0f38c2c14e4a2430135004f0b32e679dee8))
- Mark region routing complete, update real-world-conditions checklist - ([092397f](https://github.com/n0-computer/patchbay/commit/092397fd7a0a65dd06a4ab2c9d904c40741f1904))
- Mark all real-world-conditions phases complete - ([197c9fa](https://github.com/n0-computer/patchbay/commit/197c9faec2ef06d7f91c4d52c703f0d0fe4f632f))

### Rename

- Netsim → patchbay across entire workspace - ([c7d9e44](https://github.com/n0-computer/patchbay/commit/c7d9e44bf0bc6dcc53c1da5dc5d8957a6be2a4e4))


