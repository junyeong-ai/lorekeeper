# 의존성 관리

확인 기준은 **2026-10-20**이다. Rust 빌드 도구체인은
`rust-toolchain.toml`의 **1.99.1**, 지원 하한은 `Cargo.toml`의 **1.97**이다.
실제 의존성 버전은 `Cargo.lock`, 문서 전처리 패키지는
`crates/lk-source/src/content/requirements.txt`, CI 도구와 액션은
`.github/workflows/`가 관리한다. 별도의 전체 버전표를 중복 관리하지 않는다.

## 현재 선택과 제약

9개 크레이트의 직접 레지스트리 의존성 47종을 crates.io 공식 API와
대조했다. 45종은 현재 최신 안정판이 잠금 파일에 포함되어 있다.
[Tokio 1.54.0](https://github.com/tokio-rs/tokio/releases/tag/tokio-1.54.0)을
포함해 Cargo의 호환 범위 내 갱신을 적용했다.

- **HTML 변환**: htmd 0.5.6의 공개 DOM 타입과 serializer가 html5ever 0.38,
  markup5ever_rcdom 0.38에 연결된다. 두 패키지의 최신 안정판은 각각
  0.40.1과 0.39.0+unofficial이지만, 이 계열은 htmd와 함께 갱신해야 한다.
  dom_query가 사용하는 별도 html5ever 0.39를 강제 통합하지 않는다.
  [htmd 의존성](https://docs.rs/crate/htmd/0.5.6/dependencies)
- **SQLite**: rusqlite 0.40.2와 libsqlite3-sys 0.38.2의 공식 bundled 빌드는
  SQLite 3.53.2를 포함한다. SQLite upstream의 최신 안정판
  [3.54.1](https://www.sqlite.org/releaselog/3_54_1.html)과 구분한다.
  이식 가능한 공식 바인딩을 유지하며, 버전 숫자만 맞추기 위해 네이티브
  소스를 별도 포크하지 않는다. 바인딩 갱신 시 검색·스키마·재구축 검사를 한다.
- **문서 전처리**: 선택 설치되는 Kreuzberg 4.10.4, Pillow 13.0.0,
  openpyxl 3.1.5와 전이 의존성은 해시 잠금 파일로 설치한다.
  Python 3.15.0 환경에서 실제 DOCX/XLSX/PDF/이미지 처리를 확인했다.
  OCR 모델과 언어는 사용자가 명시적으로 설치·설정한다. 패키지 설치 성공을
  모든 문서의 추출 품질 보장으로 해석하지 않는다.
  [Kreuzberg v4 LTS 정책](https://docs.kreuzberg.dev/lts/)은 2026년 말까지
  중요 수정에 최선의 노력을 제공한다고 명시한다. 후속 Xberg 1.3.9는
  불변 설정·비동기 추출·타입이 있는 페이지 응답을 사용하는 별도 API다.
  격리 평가에서 기존 영문 이미지 사례가 `no-text`를 반환해 교체하지 않았다.
  전환은 기존 문서·수식·다중 페이지·영문/한국어 OCR·한도 거부 검사를 모두
  통과한 뒤 진행하며, v4 지원 종료 전에 유지보수 경로를 다시 확인한다.
- **CI**: taplo 0.10.0, nextest 0.9.148, zizmor 1.31.1,
  cargo-audit 0.22.2, cargo-deny 0.20.2, cargo-machete 0.9.2,
  cross 0.2.5를 고정한다. 확인 시점의 공식 안정 릴리스와 일치한다.
  액션은 검토한 커밋 SHA에 고정한다. 최신 안정판인 cross 0.2.5는
  2023년 배포판이므로 최신이라는 이유만으로 유지보수 상태를 보증하지 않는다.

Tokio/reqwest, Jiff, rusqlite, petgraph, MiniJinja, tracing을 교체할 근거는
확인하지 못했다. 중복 패키지 수만 줄이기 위한 런타임·DB·파서 교체나
호환성 강제 패치는 하지 않는다. Tokio/reqwest 기능 축소는 전이 의존성이
기능을 다시 활성화할 수 있으므로, 실제 빌드·용량 절감과 OAuth 콜백,
서브프로세스 및 Windows 동작이 입증되는 경우에만 별도 변경으로 진행한다.

## 갱신과 검증

`cargo update --dry-run`으로 호환 갱신을 확인하고, 비호환 후보는 upstream
변경 내용과 공개 타입 경계를 먼저 확인한다. crates.io의
`/api/v1/crates/<이름>`과 각 프로젝트의 공식 릴리스 메타데이터가 버전
대조 근거다. 새 소스 어댑터나 실행기 추가와 의존성 갱신은 별도 결정으로
관리한다.

`scripts/check.sh`가 포맷·Clippy·전체 테스트·문서·MSRV·보안·사용하지 않는
의존성·릴리스 빌드를 검사한다. Cargo 단계는 `--locked`를 사용한다.
원격 CI는 Linux/macOS 테스트와 Windows 컴파일을 추가로 확인하며,
릴리스는 정확한 태그의 CI를 통과한 뒤 빌드한다. 보안 검사는 실행 시점의
RustSec DB 판정이며, 전이 의존성 전체의 최신 여부나 취약점 부재를
영구적으로 보장하지 않는다. 설치 도구의 버전 입력은 액션 SHA 갱신과
별도로 대조한다.
