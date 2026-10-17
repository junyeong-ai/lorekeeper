# 의존성 개선 계획

기준은 **2026-10-15 23:59:59 KST**(UTC 14:59:59)다. 빌드 도구체인은 `rust-toolchain.toml`의 **1.99.1**, 지원 하한은 `Cargo.toml`의 **1.97**이다. 9개 크레이트 매니페스트, 잠금 파일, 두 워크플로와 CI 도구를 검토했다. 직접 의존성 44개는 기준일의 최신 호환 안정판을 사용한다. `markup5ever_rcdom`의 더 높은 비호환 버전은 예외로 별도 판단한다.

## 반영한 변경

로컬 `scripts/check.sh`와 CI의 Clippy·테스트·문서·컴파일 검사 및 릴리스 버전 확인용 metadata에 `--locked`를 추가했다. MSRV 검사는 로컬과 CI 모두 `--all-features`로 맞췄다. 기존 빌드의 `--locked`는 유지한다. Cargo는 디렉터리의 도구체인 파일을 사용하고, MSRV 검사만 명시적으로 `+1.97`을 선택한다. 의존성 버전과 잠금 파일은 이 변경에서 수정하지 않았다.

## 다음 작업과 완료 기준

1. **재현성 검증**: Rust 1.99.1의 전체 검사와 Rust 1.97의 모든 대상·기능 검사를 실행한다. Linux/macOS 테스트와 Windows 컴파일 검사를 통과해야 한다. CI와 릴리스는 `rust-toolchain.toml`의 channel을 읽어 설치하며, 대상 표준 라이브러리도 실제 Cargo 도구체인에 설치한다. MSRV 검사만 별도 선언한 하한을 사용한다.
2. **기능 축소 실험**: Tokio `full` 대신 실제 사용하는 fs, io-util, net, process, sync, time, rt-multi-thread, macros를 후보로 삼는다. `cargo tree -e features` 전후 비교, 릴리스 크기·깨끗한 빌드 시간 측정, OAuth 로컬 콜백·스케줄러·서브프로세스·Windows 검증 후 채택한다. 간접 의존성이 기능을 다시 활성화할 수 있어 절감량은 아직 미확인이다. reqwest 기본 기능도 같은 방식으로 검토한다.
3. **HTML 계열 유지**: htmd 0.5.6의 공개 Node 타입은 rcdom 0.38과 연결된다. rcdom만 0.39로 올리면 타입이 갈라지므로 htmd와 함께 검토한다. dom_query의 html5ever 0.39와 htmd의 0.38 중복은 강제 패치로 제거하지 않는다.
4. **도구 공급망**: cross 0.2.5는 기준일 최신 안정판이지만 2023-02-04 배포판이다. 컨테이너 이미지 digest 고정을 먼저 검토하고, 대체 도구는 두 musl 대상 및 aws-lc/blake3 네이티브 빌드 조건을 비교한 뒤 결정한다. Go·gh·러너 이미지 등 실행 환경의 실제 버전을 릴리스 기록에 남긴다. 설치 액션의 도구 버전 입력은 Dependabot의 액션 SHA 갱신만으로 관리되지 않으므로 별도 갱신 절차를 둔다.
5. **보안 확인**: 명시한 RustSec DB 커밋/날짜로 `cargo audit --deny warnings`, `cargo deny check`를 실행한다. 과거 기준일 판정과 현재 배포 시점의 권고를 구분한다. HTTP/TLS·HTML/YAML 파서·tar·SQLite·암호 라이브러리의 전이 의존성도 확인한다. 이 문서는 보안 취약점 없음이나 전이 의존성 전체의 최신 여부를 보증하지 않는다.

Tokio/reqwest, Jiff, rusqlite, petgraph, MiniJinja, tracing을 교체할 근거는 발견하지 못했다. async-trait는 동적 디스패치에 쓰이므로 네이티브 async trait로 일괄 대체하지 않는다. bundled SQLite와 테스트용 tzdb의 일관성도 유지한다. 중복 패키지 개수만 줄이기 위한 런타임·DB·파서 교체는 진행하지 않는다.

## 직접 의존성 확인표

아래 버전은 **잠금 파일의 직접 사용 버전 / 기준일 최신 안정판**이다. crates.io API의 `created_at`을 기준일과 비교했다. `+unofficial`은 prerelease가 아닌 SemVer 빌드 메타데이터다. 레지스트리의 `rust_version`이 없으면 MSRV 적합성은 빌드로 확인해야 하며, 현재 yanked 상태로 과거 시점의 상태를 단정할 수 없다.

| 직접 의존성 | 잠금 버전 | 기준일 최신 안정판 |
|---|---|---|
| [tokio](https://crates.io/api/v1/crates/tokio) | 1.53.3 | 1.53.3 |
| [async-trait](https://crates.io/api/v1/crates/async-trait) | 0.1.92 | 0.1.92 |
| [reqwest](https://crates.io/api/v1/crates/reqwest) | 0.13.5 | 0.13.5 |
| [encoding_rs](https://crates.io/api/v1/crates/encoding_rs) | 0.8.42 | 0.8.42 |
| [mime](https://crates.io/api/v1/crates/mime) | 0.3.17 | 0.3.17 |
| [serde](https://crates.io/api/v1/crates/serde) | 1.0.229 | 1.0.229 |
| [serde_json](https://crates.io/api/v1/crates/serde_json) | 1.0.154 | 1.0.154 |
| [serde-saphyr](https://crates.io/api/v1/crates/serde-saphyr) | 2.0.0 | 2.0.0 |
| [strum](https://crates.io/api/v1/crates/strum) | 0.28.0 | 0.28.0 |
| [clap](https://crates.io/api/v1/crates/clap) | 4.6.7 | 4.6.7 |
| [jiff](https://crates.io/api/v1/crates/jiff) | 0.2.38 | 0.2.38 |
| [thiserror](https://crates.io/api/v1/crates/thiserror) | 2.0.21 | 2.0.21 |
| [miette](https://crates.io/api/v1/crates/miette) | 7.6.0 | 7.6.0 |
| [tracing](https://crates.io/api/v1/crates/tracing) | 0.1.44 | 0.1.44 |
| [tracing-subscriber](https://crates.io/api/v1/crates/tracing-subscriber) | 0.3.23 | 0.3.23 |
| [minijinja](https://crates.io/api/v1/crates/minijinja) | 3.0.0 | 3.0.0 |
| [blake3](https://crates.io/api/v1/crates/blake3) | 1.8.7 | 1.8.7 |
| [semver](https://crates.io/api/v1/crates/semver) | 1.0.28 | 1.0.28 |
| [tar](https://crates.io/api/v1/crates/tar) | 0.4.46 | 0.4.46 |
| [flate2](https://crates.io/api/v1/crates/flate2) | 1.1.10 | 1.1.10 |
| [htmd](https://crates.io/api/v1/crates/htmd) | 0.5.6 | 0.5.6 |
| [markup5ever_rcdom](https://crates.io/api/v1/crates/markup5ever_rcdom) | 0.38.0+unofficial | 0.39.0+unofficial |
| [feed-rs](https://crates.io/api/v1/crates/feed-rs) | 3.0.0 | 3.0.0 |
| [dom_smoothie](https://crates.io/api/v1/crates/dom_smoothie) | 0.18.2 | 0.18.2 |
| [url](https://crates.io/api/v1/crates/url) | 2.5.8 | 2.5.8 |
| [base64](https://crates.io/api/v1/crates/base64) | 0.23.1 | 0.23.1 |
| [sha2](https://crates.io/api/v1/crates/sha2) | 0.11.1 | 0.11.1 |
| [rand](https://crates.io/api/v1/crates/rand) | 0.10.3 | 0.10.3 |
| [dialoguer](https://crates.io/api/v1/crates/dialoguer) | 0.12.0 | 0.12.0 |
| [unicode-normalization](https://crates.io/api/v1/crates/unicode-normalization) | 0.1.25 | 0.1.25 |
| [rusqlite](https://crates.io/api/v1/crates/rusqlite) | 0.40.2 | 0.40.2 |
| [pulldown-cmark](https://crates.io/api/v1/crates/pulldown-cmark) | 0.13.4 | 0.13.4 |
| [petgraph](https://crates.io/api/v1/crates/petgraph) | 0.8.3 | 0.8.3 |
| [rayon](https://crates.io/api/v1/crates/rayon) | 1.12.0 | 1.12.0 |
| [same-file](https://crates.io/api/v1/crates/same-file) | 1.0.6 | 1.0.6 |
| [walkdir](https://crates.io/api/v1/crates/walkdir) | 2.5.0 | 2.5.0 |
| [globset](https://crates.io/api/v1/crates/globset) | 0.4.20 | 0.4.20 |
| [regex](https://crates.io/api/v1/crates/regex) | 1.13.1 | 1.13.1 |
| [tempfile](https://crates.io/api/v1/crates/tempfile) | 3.27.0 | 3.27.0 |
| [insta](https://crates.io/api/v1/crates/insta) | 1.49.0 | 1.49.0 |
| [assert_cmd](https://crates.io/api/v1/crates/assert_cmd) | 2.2.2 | 2.2.2 |
| [proptest](https://crates.io/api/v1/crates/proptest) | 1.12.0 | 1.12.0 |
| [unicode-width](https://crates.io/api/v1/crates/unicode-width) | 0.2.2 | 0.2.2 |
| [emojis](https://crates.io/api/v1/crates/emojis) | 0.9.0 | 0.9.0 |

## CI 도구와 기준일 이후 항목

기준일 최신 안정판과 현재 핀이 일치하는 도구: taplo 0.10.0, cargo-nextest 0.9.148, zizmor 1.31.0, cargo-audit 0.22.2, cargo-deny 0.20.2, cargo-machete 0.9.2, cross 0.2.5. 근거는 각 패키지의 [crates.io API](https://crates.io/api/v1/crates/cargo-nextest)이며 다른 패키지도 위 표와 같은 `/api/v1/crates/<이름>` 경로로 확인했다. taplo의 패키지 이름은 `taplo-cli`다.

GitHub 공식 릴리스 메타데이터에서도 checkout v7.0.1, rust-cache v2.9.2, install-action v2.87.28, attest-build-provenance v4.2.2, upload-artifact v7.0.2, download-artifact v8.0.2, ShellCheck v0.11.0, actionlint v1.7.12가 기준일 최신 안정판과 일치했다. [checkout](https://api.github.com/repos/actions/checkout/releases?per_page=100), [rust-cache](https://api.github.com/repos/Swatinem/rust-cache/releases?per_page=100), [install-action](https://api.github.com/repos/taiki-e/install-action/releases?per_page=100), [attest](https://api.github.com/repos/actions/attest-build-provenance/releases?per_page=100), [upload](https://api.github.com/repos/actions/upload-artifact/releases?per_page=100), [download](https://api.github.com/repos/actions/download-artifact/releases?per_page=100), [ShellCheck](https://api.github.com/repos/koalaman/shellcheck/releases?per_page=100), [actionlint](https://api.github.com/repos/rhysd/actionlint/releases?per_page=100)에서 확인했다. [dtolnay/rust-toolchain](https://api.github.com/repos/dtolnay/rust-toolchain/releases?per_page=100)의 안정 릴리스는 v1이지만 이것으로 브랜치 최신 커밋을 판정할 수 없다. 2026-10-17 GitHub API로 7개 액션의 고정 SHA가 실제 존재함을 확인했다. 6개는 주석의 릴리스 태그와 정확히 일치한다. dtolnay/rust-toolchain의 SHA `6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772`는 2026-08-05 master 커밋이며, v1 태그의 `683b588c487a69d01f6c6ddf40f036ff682cc2c8`와 다르므로 워크플로 주석을 커밋 기준으로 바로잡았다. 이 액션의 고정 소스는 toolchain 입력을 필수로 요구하므로 파일에서 읽은 channel을 명시적으로 전달한다.

**기준일 이후 별도 후보**: [MiniJinja 3.0.1](https://crates.io/api/v1/crates/minijinja)은 10월 16일 00:01 KST, [zizmor 1.31.1](https://crates.io/api/v1/crates/zizmor)은 10월 16일 08:04 KST에 공개됐다. 요청한 기준일의 갱신으로 포함하지 않는다. 현재 시점에 별도 갱신한다면 릴리스 변경 내용과 템플릿/워크플로 검사를 먼저 확인한다.

잠금 파일은 레지스트리 패키지 377개 인스턴스와 중복 이름 14종을 포함한다. 예를 들어 hashbrown은 petgraph·rsqlite-vfs·hashlink/indexmap이 서로 다른 버전을 요구하고, unicode-width는 miette와 CLI가 다른 계열을 요구한다. upstream 호환 범위가 합쳐질 때 정리한다.

## 이번 변경 검증

2026-10-17에 Rust 1.99.1로 `scripts/check.sh`의 로컬 전체 검사를 통과했다. 1,377개 테스트, 문서 테스트, Clippy, Rust 1.97 전체 대상·기능 검사, 릴리스 빌드 및 셸·워크플로 검사를 포함한다. Windows 컴파일과 원격 Linux CI는 이 로컬 결과에 포함되지 않는다.

`cargo audit --deny warnings`와 `cargo deny check`도 통과했다. 사용한 RustSec DB는 `64f92cdc64401fab47baa10e2e11fe382aa57155`이며, 이는 배포 시점 검사이지 10월 15일 당시 보안 상태의 재현이 아니다. 로컬 zizmor는 1.31.1의 기본 offline 모드였고 기존 ignore 1개와 suppression 17개가 있었다. CI에 고정한 기준일 버전이나 온라인 공급망 검사 결과와 구분한다.
