# Changelog

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

## [0.3.0](https://github.com/n0-computer/patchbay/compare/v0.2.0..patchbay-v0.3.0) - 2026-03-31

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

## [patchbay-v0.1.0] - 2026-03-12

### ⛰️  Features

- *(patchbay-vm)* Add Apple Silicon (aarch64) support - ([763de72](https://github.com/n0-computer/patchbay/commit/763de72d19519efd1bed62be8ef760961f141f61))
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

- Replace events tab with lab_events file kind and DRY JSON rendering - ([e87a72e](https://github.com/n0-computer/patchbay/commit/e87a72e5463b34e10d3a4aa330c1e7613efaad7e))
- Introduce OutDir enum and recursive server scan - ([b0dd032](https://github.com/n0-computer/patchbay/commit/b0dd032ab2c6bdfbe5f6dcb8dbe391dd88b47282))
- Consolidate router presets and improve documentation - ([49d2696](https://github.com/n0-computer/patchbay/commit/49d26965dd6af3870c2fd49e6a39f7a16a016b56))
- Move simple example from patchbay-runner to patchbay - ([c7fb67a](https://github.com/n0-computer/patchbay/commit/c7fb67af9cd17267dc3f9f2571534384fc00ea29))
- Improve lib.rs docs, remove ObservedAddr, tighten visibility - ([2770ff4](https://github.com/n0-computer/patchbay/commit/2770ff41cf95c3ba36d7b5c7755eae9e84c0e4d8))

### 📚 Documentation

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

- Add runner sim e2e test - ([917e5af](https://github.com/n0-computer/patchbay/commit/917e5afb765408c328cb021fdd3488d6116cb06e))

### ⚙️ Miscellaneous Tasks

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


