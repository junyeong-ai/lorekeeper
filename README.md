# Lorekeeper

[![Rust](https://img.shields.io/badge/rust-1.97%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![DeepWiki](https://img.shields.io/badge/DeepWiki-junyeong--ai%2Florekeeper-blue?style=flat-square)](https://deepwiki.com/junyeong-ai/lorekeeper)

> **[English](README.en.md)** | **한국어**

**흩어진 일상의 작업을 스스로 자라는 지식 위키로.**
Gmail·Slack·Jira·캘린더·RSS·메모를 매일 모아 중복을 없애고, 개념을 추출하고, Obsidian 마크다운으로 정리합니다. LLM이 *정리·연결·갱신(부기)* 을 대신하니 지식은 방치되지 않고 **복리로 쌓입니다.**

---

## 왜 Lorekeeper인가?

메모·위키가 실패하는 진짜 이유는 *읽기·생각하기*가 아니라 **부기(bookkeeping)** — 교차참조 갱신, 중복 정리, 분류, 모순 점검 — 의 부담입니다. 사람이 이걸 못 버텨서 위키를 방치하죠. Lorekeeper는 그 부기를 LLM에게 맡깁니다.

> 📖 **처음 보는 용어들** — **vault**: 마크다운 파일이 모인 폴더(곧 당신의 지식 저장소) · **개념(concept)**: 한 주제당 한 페이지 · **링크**: 페이지를 잇는 `[표시 이름](상대/경로.md)` 마크다운 링크(Obsidian·GitHub·OKF 어디서든 해석) · **부기**: 링크·중복·분류를 손으로 갱신하는 그 잡일.

| | |
|---|---|
| 📥 **설정 한 번 → 매일 자동** | 메일·메신저·이슈·일정·피드·메모에서 어제의 활동·지식을 수집 |
| 🧹 **노이즈 제거** | 중복 차단, 관련 없는 항목 필터, 내 일은 work-log로 자동 분리 |
| 🧩 **개념 자산화** | 같은 개념은 한 페이지로 수렴(`Vector DB` = `vector-database`), 카테고리·연관관계 정리 |
| 🔗 **연결되는 지식 그래프** | 마크다운 링크·역링크·클러스터로 개념이 서로 이어짐 |
| ✅ **할 일 → 기록** | 완료한 할 일과 메모를 보존하고, 설정된 tasks 소스의 수집·의미 처리를 거쳐 데일리·개념·성과 리뷰에 활용 |
| 📈 **복리** | 주간·월간·분기·연간 합성으로 시간이 갈수록 가치↑ |
| 🔑 **CLI 로그인 재사용** | Claude Code·Codex의 기존 인증 사용; Lore의 별도 LLM API 클라이언트 불필요 |

> 💡 영감: Andrej Karpathy의 *"LLM이 관리하는 위키"* — **출처와 생성된 지식을 구분**, **위키는 LLM이 작성·유지**, **스키마(config)가 워크플로를 정의**. 사람은 소스를 고르고 질문하고, LLM은 부기를 한다.

---

## 한눈에 보기

```mermaid
flowchart LR
    subgraph SRC["📡 데이터 소스"]
        direction TB
        G["Gmail"]
        S["Slack"]
        J["Jira"]
        C["Calendar"]
        R["RSS"]
        M["메모 inbox"]
    end
    SRC --> L["⚙️ lore (Rust CLI)"]
    L --> P["수집 · 정규화 · 중복제거<br/>분류 · 렌더 · 그래프"]
    P --> V[("🗂️ Obsidian Vault<br/>daily · concepts · documents")]
    L -. "LLM 작업 큐" .-> CC["🤖 Claude Code · Codex<br/>공통 lore-process 스킬"]
    CC -. "타입이 있는 의미 결과" .-> L
```

`lore`가 수집·실행 순서·게시 검증을 맡고, 공통 `lore-process` 스킬을 실행하는 Claude Code 또는 Codex가 요약·개념·관계를 판단합니다. Markdown이 지식 원본이며 SQLite는 다시 만들 수 있는 검색·탐색 인덱스입니다.

---

## 빠른 시작

> **시작하기 전에 준비물** — 어렵지 않습니다:
> - 💻 **macOS 또는 Linux** (Windows는 PowerShell 설치 스크립트 제공)
> - 🤖 **Claude Code 또는 Codex** — 기존 CLI 로그인으로 요약·개념·관계를 처리합니다
> - 🔑 **쓰려는 소스의 자격증명만** — 예: Gmail/Drive/Calendar는 Google 로그인, Slack은 토큰
> - 🗂️ **(선택) Obsidian** — 그래프를 예쁘게 탐색용. 없어도 결과물은 평범한 마크다운입니다
>
> 💡 기본 시작 설정은 **인증이 필요 없는 메모 inbox 하나**입니다. RSS·업무 소스·개인 활동 추적은 필요할 때 추가합니다. 요약·개념 생성에는 선택한 CLI의 로그인이 필요합니다. 복사한 `config.yaml`의 `execution.profiles.primary.engine`을 `claude-code` 또는 `codex`로 설정하세요.

```bash
# 1) 설치 — 바이너리 + 템플릿 + 두 호스트의 스킬을 한 번에
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.sh | bash

# 2) 처음 설정 — 기존 config.yaml이 있으면 보존하고 편집
LORE_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/lorekeeper"
cp -n "$LORE_CONFIG_DIR/config.starter.yaml" "$LORE_CONFIG_DIR/config.yaml"
${EDITOR:-vi} "$LORE_CONFIG_DIR/config.yaml"  # vault.root, timezone, locale, identity 확인

# 3) 로컬 검증과 지식 작성 규약 생성
lore validate
lore schema
mkdir -p "$(lore config vault-root)/inbox"
```

vault의 `inbox/`에 메모 파일 하나를 넣은 뒤 수집합니다. `inbox_dir: inbox`는 변경한
`vault.root`를 따라갑니다. 기본 설정에는 공급자 인증 마법사가 필요하지 않습니다.

```bash
# 4) 입력 접근을 확인한 뒤 실제 수집
lore ingest --dry-run
lore run daily
```

`lore run daily`는 수집 후 선택한 실행기로 요약·개념·관계를 처리하고 게시를 검증합니다.
수집만 끝난 상태는 지식화 완료가 아닙니다. 처리가 끝나면 다음을 확인합니다.

```bash
lore queue check
lore wiki reviews --check
lore wiki search "메모의 주제" --json

# 5) 자동화가 필요하면 예약 정의 생성 후 OS에 등록
lore schedule > lorekeeper.cron
# 기존 예약을 보존하며 병합하고 실제 실행 확인. macOS는 아래 스케줄링 절 참고
```

`config.example.yaml`은 전체 옵션 카탈로그이며 모든 소스가 비활성 상태입니다.
필요한 블록만 실제 `config.yaml`에 추가하고 범위·계정 값을 채운 뒤 활성화하세요.
`/lore-setup`은 이 선택과 ID 탐색을 돕습니다. 인증이 필요한 소스를 추가하면
`lore init credentials` → 해당 소스의 `lore ingest --dry-run <source-id>` 순서로 확인합니다.
예약 정의 생성·OS 등록·실제 실행은 각각 별도 단계입니다. [스케줄링](#스케줄링)을 따르세요.

> Obsidian이 없어도 됩니다 — 결과물은 평범한 마크다운 + 폴더라 그냥 텍스트로 읽힙니다. Obsidian은 그래프 탐색을 예쁘게 보여줄 뿐.

---

## 실제로 어떤 결과가 나오나요?

가상의 시나리오로 따라가 봅니다. AI 엔지니어 **수민**이 RAG 트러블슈팅을 정리한 메모를 vault의 `inbox/`에 떨어뜨립니다.

> 아래 페이지 예시는 **핵심 필드만 발췌**했습니다 — 실제 파일에는 `updated` 같은 관리용 frontmatter도 함께 들어갑니다.

### 📝 입력 — `inbox/rag-검색-품질.md`

```markdown
# RAG 파이프라인: 낮은 recall 검색 고치기

문제: 질의의 30%가 무관한 청크를 검색해 생성기가 환각을 일으켰다.
원인: (1) 청크가 2000토큰으로 너무 커서 임베딩이 여러 주제의 평균이 됨.
      (2) 대화형 질문("그 다른 거는?")을 그대로 임베딩.
해결: 청크를 ~400토큰으로 줄이고, 질문을 standalone 쿼리로 rewrite하고,
      유사도를 L2→cosine으로 변경. Recall@5: 0.62 → 0.91.
```

### ▶️ 실행

```console
$ lore ingest
▸ notes (manual)
  extracted: 1 items
  ✓ wrote: wiki/documents/rag-파이프라인-낮은-recall-검색-고치기.md (document)

Done. 1 pages written, 0 personal items tracked.
```

이 시점에 페이지는 만들어졌지만 `## 요약`·`## 관련 개념`은 비어 있습니다 — `lore ingest`가 LLM 작업을 큐에 넣었기 때문입니다:

```console
$ lore queue status
  [current] sum-… (summarize)        → wiki/documents/rag-파이프라인-…
  [current] ext-… (extract-concepts) → wiki/documents/rag-파이프라인-…
queue: 2 current, 0 done, 0 stale, 0 missing-target, 0 unreadable across 2 task(s)
```

이제 **`lore run process`**를 실행합니다. 설정된 Claude Code·Codex 실행기가 공통 스킬로 큐를 처리하고, Rust가 출처와 판본을 검사해 게시합니다. 끝나면 `lore queue status`는 `0 current`가 되고 페이지가 채워집니다:

### ✅ 결과 ① — 문서 페이지 (요약 + 개념 링크가 채워짐)

```markdown
---
id: rag-파이프라인-낮은-recall-검색-고치기
type: document
title: "RAG 파이프라인: 낮은 recall 검색 고치기"
created: 2026-06-13
tags: ["document"]
---

## 요약
청크를 2000→400 토큰으로 줄이고, 대화형 질문을 standalone 쿼리로 재작성하고,
유사도를 L2→cosine으로 바꿔 이 메모의 Recall@5가 0.62 → 0.91로 개선되었다. 세 변경을 함께 적용한
관찰이므로 각 변경의 개별 효과나 다른 환경에서의 우열은 확인되지 않았다.

## 내용
… (원본을 정규화해 보존) …

## 관련 개념
- [Retrieval-Augmented Generation](../../wiki/concepts/retrieval-augmented-generation.md)
- [벡터 데이터베이스](../../wiki/concepts/vector-database.md)
- [청킹](../../wiki/concepts/청킹.md)
- [질의 재작성](../../wiki/concepts/질의-재작성.md)
```

### ✅ 결과 ② — 개념 페이지가 **수렴**합니다 (핵심 가치)

며칠 뒤 수민이 *"프로덕션 벡터 DB 선택"* 메모를 또 드롭합니다. "벡터 데이터베이스"는 두 메모 모두에 등장하지만 — **새 페이지를 만들지 않고 기존 개념에 합류**합니다:

```markdown
---
id: vector-database
type: concept
title: "벡터 데이터베이스"
aliases: ["벡터 데이터베이스", "Vector DB"]
category: ai-ml
source_count: 2          # ← 두 문서가 이 한 개념을 인용
---

## 핵심
벡터 데이터베이스는 고차원 임베딩을 저장하고 근사 최근접 탐색(ANN)을 제공한다.
두 번째 메모의 기존 PostgreSQL 환경에서는 별도 시스템의 운영 부담을 줄이기 위해
pgvector를 선택했다. 이 결정의 조건과 첫 메모의 검색 품질 관찰은 별도로 유지한다.

## 출처
<!-- lorekeeper:concept-sources:v1 -->

전체 출처는 `lore wiki prepare`로 스냅샷 ID를 받은 다음 `lore wiki citations EXACT_CONCEPT_PATH --snapshot ID`로 조회합니다.
```

> `Vector DB`라고 쓰든 `벡터 데이터베이스`라고 쓰든 **한 페이지**로 모입니다(alias로 등록). 같은 지식이 흩어지지 않는 것 — 이게 "복리로 쌓인다"의 핵심입니다.

소스별 `concept_policy`로 개념 증가를 제어합니다. `disabled`는 원문·요약만 수집하고,
`existing-only`는 기존 개념에만 연결하며, `supported-new`는 근거가 있는 새 개념도 허용합니다.
생략하면 `disabled`입니다. `focus`는 관심 범위를 별도로 제한합니다. 예를 들어 반복 알림은
`existing-only`, 재사용할 설계 지식이 있는 문서는 `supported-new`로 설정할 수 있습니다.
이 정책은 작업 해시에 포함되고 Rust 적용 단계에서도 검사합니다.

### ✅ 결과 ③ — 필요한 만큼 펼쳐 보는 탐색

```bash
lore wiki snapshot                          # 저장된 검색 판본·마지막 검증 시각 조회
lore wiki prepare                           # 현재 원본을 검증하고 검색 판본 생성
lore wiki search "Vector DB" --snapshot <id> --limit 10 --json
lore wiki context wiki/concepts/vector-db.md --snapshot <id> --limit 10 # 관계별 근거를 나누어 탐색
lore wiki browse --type concept --category ai-ml --limit 20 --json
lore wiki search "API 토큰" --phrase --snapshot <id> --json
lore wiki search "인증정보" --spacing --snapshot <id> --json
lore wiki browse --tag security --tag 인증정보 --snapshot <id> --json
lore wiki read wiki/concepts/vector-database.md # 목차와 revision 확인 → 필요한 section 선택
lore wiki links wiki/concepts/vector-database.md --limit 20
lore wiki citations wiki/concepts/vector-database.md --snapshot <id> --limit 20
lore wiki reviews        # 현재 미해결 판단을 별도로 확인
```

Markdown이 지식의 정본이고 `.lorekeeper/search.sqlite`는 재생성 가능한 탐색 계층입니다.
처음에는 이름·별칭·짧은 본문·출처 식별 정보로 후보를 좁히고, 선택한 페이지의 절·관계·근거·보존 원본을 차례로 읽습니다.
본문의 정확한 문자열과 정규식은 `wiki search --body`와 `--regex`로 찾습니다.
등록된 이름의 띄어쓰기 차이는 `name-variant` 후보로 찾으며 정확한 이름 소유권은 바꾸지 않습니다.
`--phrase`는 연속 구절을, `--spacing`은 같은 본문 블록 안의 띄어쓰기 차이를 검색합니다. 별도 문단·목록 항목·표 셀을 이어 붙이지 않습니다.
HTML 표는 셀 순서와 빈 셀을 보존하며, 병합 셀·중첩 표는 HTML 구조로 유지합니다. 이 표의 텍스트도 같은 경계로 검색하고, 태그·링크 주소·스크립트·스타일은 검색 본문에서 제외합니다.
태그는 주제별 AND 필터이며 보존된 수동 원문의 태그도 포함합니다. 별칭은 같은 대상의 이름이고 태그는 분류이므로 서로 대신하지 않습니다.
검색 순위나 출처 수는 사실의 신뢰도 점수가 아닙니다.
`wiki citations`는 그래프가 도출한 정확한 개념 근거를 SQLite에서 조회합니다. 일반 링크 목록과
구분되며, RSS는 선택한 기사와 원본 판본을 보존합니다. `total`은 근거 항목 수이고
`source_count`는 출처 페이지 수입니다. 같은 snapshot과 `next_offset`으로 전체를 이어서 읽습니다.
현재 원문을 다시 읽지는 않으므로, 판본이 바뀌었으면 다시 준비하고 선택한 원문을 검증하세요.
개념의 출처 절에는 고정된 조회 안내만 두어 원문 수가 늘어도 본문이 커지지 않습니다.
안내의 `EXACT_CONCEPT_PATH`를 현재 개념 주소로 바꿔 조회합니다. 기존의 정확한 생성 목록만
이 안내로 전환하며, 별도로 작성한 출처 설명은 보존하고 보고합니다. 전환으로 실제 본문
판본이 바뀐 기존 검토는 다시 확인할 대상으로 남습니다. 이후 출처 증가는 안내 본문을 바꾸지 않습니다.

기본 검색은 활성 지식만 보여 줍니다. `--knowledge-status retired|all`로 비활성 항목을 조회할 수 있으며,
주소·본문·인용·판단 이력은 보존됩니다. 출처 하나나 오래된 날짜만으로 비활성화하지 않습니다.
결과의 `reviews`는 해당 지식을 평가하는 검토와 그 근거 판본의 변경 여부를 보여 줍니다.
검토 주제를 지정하는 `review.subjects`는 인용이나 독립 증거 수를 늘리지 않습니다.

`--snapshot`은 준비 시점의 내용을 조회합니다. 변경 후 다시 준비하고, 근거를 읽을 때는 반환된 revision으로 확인합니다.
페이지/링크 목록은 `next_offset`으로 이어서 읽습니다. 스냅샷 링크 조회에는 같은 snapshot을 사용하며 실시간 `--revision`을 함께 지정하지 않습니다.

`index.md`는 작은 진입 안내이며, 목록·작성일순 탐색·링크와 역링크는 SQLite 검색 스냅샷으로 조회합니다.
`--snapshot`을 생략한 검색·목록 조회는 현재 Markdown을 읽습니다. 작성일순 조회의 `created`는 수집일과 다를 수 있습니다.
Obsidian에서는 원래 지식 페이지의 링크와 검색을 그대로 사용합니다.

충돌은 같은 조건에서 서로 양립할 수 없는 주장인지 근거를 읽고 판단합니다. 날짜가 최신이거나 출처가 많다는 이유로 자동 해결하지 않습니다.
`wiki reviews`로 현재 열린 검토를 찾고, 증거가 바뀐 해결 기록은 과거 판단을 보존한 채 재검토합니다.
예약 파이프라인의 `queue check`와 `wiki reviews --check`는 미완료 작업·증거 변경을 검사하지만, 의미적 진실 전체를 증명하지는 않습니다.

실제 재검토 전에는 `lore wiki evidence --archive <검토 경로>`로 이전 판단을 보존합니다.
새 근거를 읽고 보관한 다음 `review.evidence`를 명시적으로 교체하고, 날짜·판단·이전 보관본 경로를 본문에 기록합니다.
판단을 재검토하지 않은 채 revision만 새 값으로 바꾸지 않습니다.

### ✅ 결과 ④ — 지식 그래프가 형성됩니다

`lore graph cluster`는 군집을 만들고, `suggest-links`는 공동인용을 차수로 정규화한 Adamic-Adar 점수로 연결 후보를 제안합니다. 의미 관계는 원문 근거를 검토해 확정합니다:

```mermaid
graph TD
    RAG["RAG"]:::ai
    VDB["벡터 데이터베이스"]:::ai
    EMB["임베딩"]:::ai
    CHK["청킹"]:::ai
    QR["질의 재작성"]:::ai
    COS["코사인 유사도"]:::ai
    PG["pgvector"]:::infra
    HN["HNSW"]:::infra

    RAG --- VDB
    RAG --- CHK
    RAG --- QR
    VDB --- EMB
    VDB --- PG
    VDB --- HN
    EMB --- COS
    CHK --- EMB

    classDef ai fill:#eef2ff,stroke:#6366f1,color:#1e1b4b
    classDef infra fill:#ecfeff,stroke:#06b6d4,color:#083344
```

### 📈 시간이 지나면 — 복리

```mermaid
flowchart LR
    D["매일<br/>일일 수집 + 개념 축적"] --> W["주간<br/>테마 합성 · 성과 리뷰"]
    W --> Mo["월간<br/>리뷰"]
    Mo --> Q["분기<br/>리뷰"]
    Q --> Y["연간<br/>리뷰"]
    style D fill:#f0fdf4,stroke:#22c55e
    style Y fill:#fef2f2,stroke:#ef4444
```

매일의 수집이 개념 그래프를 키우고, 합성이 그것을 점점 높은 고도에서 요약합니다. 일일 → 주간 → 분기 → 연간으로 갈수록 **이미 쌓인 것을 다시 쓰지 않고** 그 위에 누적됩니다.

---

## 소스

| 타입 | 용도 | 인증 |
|---|---|---|
| `gmail` | 메일 다이제스트 (라벨/발신자로 필터) | Google OAuth |
| `slack-channel` | 채널 전체 = 팀 활동 (스레드·봇필터·watch_users) | Slack 토큰 |
| `slack-search` | 키워드 트렌드 검색 | Slack user 토큰 |
| `jira` | 그날 작업한 이슈 스냅샷 (ADF→Markdown) | Jira API |
| `confluence` | CQL로 고른 스페이스·페이지 (스토리지 포맷→Markdown) | Atlassian OAuth 또는 API 토큰 |
| `google-calendar` | 일정 + 회의록(Drive 링크 자동 추출) | Google OAuth |
| `google-drive` | Drive 폴더의 큐레이션 문서 | Google OAuth |
| `rss` | 벤더 블로그·뉴스 → 개념 (인증 불필요, 다중 피드) | 없음 |
| `nodex` | 프로젝트 저장소가 직접 쓴 결정·학습 기록 → 개념 (`nodex.toml`과 nodex `>=0.60,<0.61` 필요; [실행 파일·근거 버전 설정](skills/lore-setup/references/config.md#nodex-binary-selection)) | 없음 |
| `manual` | `inbox/`에 드롭한 마크다운·텍스트·HTML 파일 | 없음 |
| `tasks` | 내가 끝낸 작업 — `lore task`로 닫은 항목이 그날의 페이지가 됨 | 없음 |

소스 키 = vault의 하위 폴더 이름. 같은 타입을 여러 개 정의할 수 있습니다(예: `team-slack`, `ai-news`). 전체 예시는 [`config.example.yaml`](config.example.yaml).

외부 지식을 추가하려면 카탈로그의 `ai-news` 또는 `tech-news` 블록을 실제 설정에 복사해 활성화하세요. 공식 발표·모델 기법·AI 구현·제품 평가와 국내 개발 사례를 기본으로 두고, 추가 연구소·뉴스레터·집계 피드는 선택하도록 나눴습니다. `fetch_full_text`는 짧은 피드의 본문을 가져오지만 접근 실패 시 피드 텍스트만 남습니다. `focus`는 요약·개념 추출의 범위이며 원본 수집을 차단하지 않습니다. 피드의 현재 목록과 조회 한도는 전체 과거 이력이나 사실의 정확성을 보장하지 않습니다.

---

## 핵심 개념

- **결과물은 평범한 마크다운** — `daily/{소스}/`(원본 타임라인), `wiki/concepts/`(개념), `wiki/documents/`(문서), `me/`(work-log·성과리뷰), `synthesis/`(주간 테마).
- **자료화된 뷰(materialized view)** — 페이지는 두 층. **구조 층**(frontmatter·원본·헤딩)은 매 수집마다 재생성, **의미 층**(요약·개념·합성)은 LLM 소유이며 재렌더에도 보존됩니다. 입력이 안 바뀌면 LLM 작업 0건(BLAKE3 해시로 판정).
- **입력과 재처리** — 입력 해시가 같으면 완료된 LLM 결과를 재사용합니다. 재수집은 원격 수정·삭제와 조회 범위의 영향을 받습니다. RSS 이벤트 로그는 피드에서 밀려난 수집 항목을 보존하며, 과거 입력 그대로의 재합성은 보관된 큐 입력이 있을 때만 가능합니다.
- **현재만 실체화** — 미래 날짜는 페이지를 만들지 않음(forecast는 지식이 아님). 날짜가 오면 지식이 됩니다.
- **vault의 언어는 설정이 정한다** — `vault.locale`(ko | en)은 라벨만 바꾸는 스위치가 아니라 **Lorekeeper가 덧붙이는 모든 문장의 언어**입니다. 요약·주간 테마·개념 정의·탐구 페이지가 그 언어로 쓰이고, 개념의 이름은 그 개념을 쓴 출처가 적은 형태를 그대로 따릅니다 — 번역하지 않으므로, 출처가 다른 언어로 적은 용어는 그 철자를 유지합니다. 원문도 번역하지 않습니다.
- **그래프가 부기를 한다** — `backlinks-sync`(개념의 인용·카운트·요약 입력 재도출, 근거가 바뀐 요약은 큐로), `lint`(고아·깨진링크·중복개념), `merge`(중복 개념 통합), `cluster`/`suggest-links`(군집·연결 후보 제안).

---

## 명령어

```bash
lore validate                 # config 점검 (네트워크 없음)
lore ingest [소스]            # 수집 (전체 또는 단일 소스)
lore ingest --dry-run         # vault 변경 없이 미리보기
lore ingest --date 2026-06-01 # 특정 날짜 재실체화(백필/복구)
lore synthesis weekly         # 주간 합성 + 개인 리뷰 (monthly/quarterly/annual)
lore status                   # 하위시스템별 한 줄 요약 (소스별 시각은 lore health)
lore health                   # 수집이 밀린 소스 경고 (ingest.schedule 기준)
lore health --json            # 같은 판정을 계약으로 — 스킬이 읽는 형태
lore schedule                 # 예약 정의 출력 — OS 등록은 별도
lore brief --date yesterday   # 그날의 지식 — 새로 안 것(한 줄씩)과 다시 본 것(이름만)
lore fetch <url>              # 링크 하나를 읽어 inbox에 저장(출처 주소를 달아서)
lore wiki search <질의>       # 질의가 닿는 페이지 — 이름으로 맞았는지 본문에서 맞았는지까지
                              # 등록된 alias로 쓰인 본문까지 검색 (`지식그래프`↔`지식 그래프`)
lore wiki browse --category ai-ml --type concept --limit 20 --json # 분류별 탐색; next_offset으로 다음 페이지
lore graph --json neighbors wiki/concepts/example.md # 출처를 포함한 양방향 링크 탐색
lore graph --json neighbors wiki/concepts/example.md --type concept # 관련 개념만 탐색; --type document로 원문 조회
lore wiki snapshot            # 원본 재탐색 없이 저장된 판본 조회
lore wiki prepare             # 현재 원본 전체 검증; 반복 검색에 --snapshot <id> 사용
lore wiki search "정확한 문자열" --body --json # 원문 행·문맥·판본; 정규식은 --regex
lore wiki search "정확한 문자열" --body --snapshot <id> --json # 준비한 판본에서 반복 검색
lore wiki document-identity plan --source ID --manifest /tmp/identity.json # 기존 사본의 식별자 이관 검증
lore wiki read wiki/concepts/example.md # 목차 → --section ID; 후속 읽기에 --revision 사용
lore wiki original wiki/documents/example.md # 보존된 수동 원본·첨부 목록; --member로 선택 조회
lore wiki original daily/tech-news/2026-10-18.md # RSS 수집 원문 목록·authority·revision
lore wiki original daily/tech-news/2026-10-18.md --revision REV --event EVENT_ID --text-offset 0 --text-limit 4096
lore wiki links wiki/concepts/example.md --limit 20 # 연결된 원문 행·설명·리비전 조회
lore wiki citations wiki/concepts/example.md --snapshot <id> --limit 20 # 정확한 전체 근거를 페이지 단위로 조회
lore wiki provenance wiki/documents/example.md --snapshot <id> # 정확한 문서 ID로 선언 관계·후속 문서 조회
lore wiki curate --file plan.json --dry-run # 근거 판본·이름·분류·태그·검토 기록 검증; 옵션을 빼면 원자적으로 반영
lore wiki concepts            # 개념 목록
lore resolve <name>           # 어떤 개념 페이지가 그 이름을 갖는지 (0 소유 / 1 없음 / 2 중복)
lore graph lint               # 구조 건강검진(고아·깨진링크·중복개념·…)
lore graph suggest-links --limit 20 # 관계 후보; next_offset과 --revision으로 이어서 조회
lore graph cluster            # 토픽 커뮤니티(Louvain)
lore graph backlinks-sync     # 개념의 ## Sources·인용수·요약 입력 재도출(변한 요약은 큐로)
lore graph merge <from> <into># 중복 개념 통합
lore graph retract-citation <원문.md> <개념.md> --revision <file_revision> --dry-run # 검토한 오인용 철회 미리보기
lore graph normalize --fix    # 링크 표기 정규화
lore doctor                   # 페이지 계약 감사(텍스트 청결도·미답변 섹션·자격증명)
lore extract status           # 프로젝트 지식 추출이 저장소보다 얼마나 뒤처졌는지
lore maintenance              # 지난 운영 이력·게시 완료 작업 캐시 정리; 미완료·참조 중 작업 보존
lore queue status / prune     # LLM 작업 큐 상태 / 죽은 작업 정리
lore queue apply              # 검증된 요약·테마·정제·개념 결과를 중앙 적용
lore queue check              # 미완료 작업·미게시 결과·중단된 변경을 JSON으로 검사
lore wiki reviews --check     # 증거 변경·잘못된 검토 검사(미해결 검토는 통과)
lore queue packet --task-id ID # 병렬 작업용 출처·revision 묶음 (본문은 --include-content)
lore queue regenerate --task-id ID --dry-run # 보관 입력 재합성 미리보기
lore queue count              # current 작업 수만 정수로 출력(스크립트용)
lore work plan --task-id ID    # 대용량 보관 입력의 구간 처리 계획 (감사는 --path)
lore work status WORK_ID      # 완료 구간·재개할 작업 확인
lore work read WORK_ID NODE_ID # 해당 구간 또는 두 하위 결과만 읽기
lore work result WORK_ID      # 완료된 통합 결과를 최종 게시용으로 조회
lore config vault-root        # vault 절대경로만 출력(스크립트용)
lore schema                   # wiki/AGENTS.md(페이지 포맷 스키마) 생성
```

`lore fetch`는 HTTP `Content-Type`에 따라 HTML·Markdown·일반 텍스트를 읽습니다. `text/plain`으로 제공되는 `.md`·`.markdown` 파일도 Markdown으로 보존하며, 링크·이미지의 상대 주소는 최종 응답 URL을 기준으로 바꿉니다. 형식 선언이 없거나 지원하지 않는 응답, 잘못된 문자 인코딩과 16 MiB를 넘는 본문은 오류로 보고합니다. Markdown 안의 raw HTML 주소는 그대로 남아 수동 원본의 의존성 검사 대상이 됩니다.

`lore wiki refresh`는 SQLite 탐색 스냅샷과 작은 `index.md` 진입 안내를 갱신합니다. 지식 목록·날짜순 조회·관계 탐색은 `wiki browse/search/links`로 수행합니다. 생성 마커가 없는 진입 파일은 보존하고 충돌을 보고합니다. 탐색 세션은 `wiki snapshot`으로 저장된 ID와 생성·검증 시각을 조회한 뒤 `wiki browse/search/links --snapshot ID`를 반복하고(`links --direction incoming|both`), `wiki read --revision REV`로 현재 원문을 확인합니다. 현재 문서 목록이 필요하거나 호환되는 판본이 없으면 `wiki prepare`로 전체 원본을 검증합니다. 저장된 판본의 시각은 외부 편집 이후의 최신성을 보장하지 않습니다. refresh도 준비한 ID를 출력합니다.

프로젝트 문서의 `source_record.attributes`는 nodex 공개 응답의 원본 속성을 보존합니다. routine의 `prompt`처럼 본문 밖에 있는 지식도 요약·추출 입력에 포함하며, 속성은 현재 작업을 실행하라는 지시가 아닌 출처의 내용으로 읽습니다.

새 RSS ingest는 분류 전 수집 텍스트를 `.lorekeeper/evidence/rss/`에 불변 해시 주소로 보존하고 daily의 `source_record.rss_original`로 연결합니다. 이는 공급자의 HTTP/XML 원본이 아니라 adapter가 취득한 본문이며, `event_timestamp`는 published 또는 updated에서 온 adapter 시각이며 취득 시각이 아닙니다(`captured_at: null`). `wiki original`은 목록을 먼저 반환하고, 반환된 `revision`과 정확한 `event_id`로 제목·빈 줄·본문을 문자 단위 페이지네이션합니다. 새 원문은 `page_bound_collection`, 과거 이벤트 로그의 읽기 전용 발견은 `legacy_unbound`입니다. 후자는 해당 daily의 과거 입력임을 증명하지 않습니다. 요약은 해석이며 원문의 부재를 증명하지 않습니다. 공급자 URL을 다시 읽은 내용도 당시 수집본과 같다고 볼 수 없습니다. 새 bound RSS 추출의 각 컨셉은 LLM이 명시적으로 선택한 `event_id`와 immutable payload revision을 `concept_evidence`에 저장해야 합니다. 기사 선택이 없으면 게시를 거부하고, 추출 결과가 빈 목록이면 정상 완료할 수 있습니다. 이 선택만 기사 단위 합성 근거가 되어 무관한 일일 요약 변경으로 재합성되지 않습니다. 과거 페이지와 귀속 없는 링크는 기존 페이지 guard를 유지합니다. `wiki original ORIGIN --evidence-concept CONCEPT`로 선택 목록을 읽고 `--event ID --revision PAYLOAD_REV`로 당시 기사를 확인합니다. 최신 manifest와 다른 과거 선택도 읽을 수 있으며, 현재 사실이나 독립 검증을 보장하지는 않습니다. 추출의 누락은 철회가 아니고, `graph retract-citation`의 `--event`/`--event-revision`으로 명시 철회합니다. `source_count`는 계속 원문 페이지 수입니다. 원문 저장 실패 시 페이지는 그 원문을 참조하지 않으며 dry-run은 보관하지 않습니다.

날짜별 탐색은 `lore wiki browse --snapshot ID --sort created --from 2026-01-01 --to 2026-12-31 --json`을 사용합니다. `created`는 수집일이 아니라 원문 작성일일 수 있습니다. 날짜가 없거나 유효하지 않으면 상태를 표시하고 날짜 필터에서는 제외합니다. 날짜순은 최신부터이며, 필터가 없으면 날짜 미상 항목을 마지막에 둡니다.

일상적으로 쓰는 것들만 추렸다. 전체 목록은 `lore --help`, 각 명령의 플래그는
`lore <명령> --help`.

---

## 에이전트 스킬과 실행 설정

스킬 본문과 참고 자료의 원본은 `skills/`입니다. Claude 전용 메타데이터는
`agents/claude.json`, Codex 호출 정책은 필요한 스킬의 `agents/openai.yaml`에 둡니다.
배포할 때 호스트별 진입 파일을 만들며, 공통 절차를 따로 편집하지 않습니다.

| 스킬 | 하는 일 |
|---|---|
| `lore-process` | 큐의 요약·개념·합성·검토 결과 처리와 게시 검증 |
| `lore-setup` | 소스·설정·인증·최초 실행·예약 등록 안내 |
| `lore-wiki` | `query`, `add`, `capture`, `audit`, `curate`, `maintain`, `status` |
| `lore-extract` | 저장소 지식 추출: scan → run → audit |
| `lore-ingest` | 수집·합성·상태·스케줄 CLI 작업 |
| `lore-day` | 할 일·브리핑·리마인더·마감 |

Claude Code에서는 `/lore-wiki query ...`, Codex에서는 `$lore-wiki`로 사용합니다.
`lore self deploy --skills user --agent codex`는 Codex에만 대화형 스킬을 설치합니다.
`--agent both`는 두 호스트에 설치하며, `--skills project`는 현재 프로젝트 범위입니다.
Claude 경로는 `.claude/skills`, Codex 경로는 `.agents/skills`입니다.
무인 실행은 `<data>/skills/<engine>/`의 검증된 배포본을 사용합니다.
누락·변조된 필수 파일은 세션 시작 전에 거부합니다.

실행기 지원 범위는 Claude Code `>=2.1.259,<3`, Codex CLI `>=0.163.0,<1`입니다.
호스트의 기존 인증과 정책을 사용합니다. 두 제품의 권한·샌드박스는 서로 다르며,
Rust의 게시 검증을 OS 접근 통제와 같은 보장으로 취급하지 않습니다.

```yaml
llm:
  provider: queue
execution:
  default_profile: primary
  work:
    max_input_bytes: 262144
    max_result_bytes: 32768
    max_calls: 64
  profiles:
    primary:
      engine: claude-code
      timeout_seconds: 3600
    reviewer:
      engine: codex
      timeout_seconds: 3600
  operations:
    knowledge: reviewer
```

이 예에서는 일일 처리·큐 처리는 `primary`, 의미 감사는 `reviewer`를 사용합니다.
`--profile` 지정 → 작업별 설정 → 기본 프로필 순서로 선택하며 다른 실행기로 자동 전환하지 않습니다.
`timeout_seconds`는 세션별 실행 예산입니다. 미완료 작업은 보존하고 실패로 보고합니다.
`execution.work`는 작업자 요청 JSON과 관찰 결과 JSON의 바이트 수, 실행당 의미 처리 세션
호출 수를 제한합니다. 큰 입력은 판본이 고정된 조각으로 나누어 새 세션에서 읽고, 관찰 결과도
새 세션에서 단계적으로 종합합니다. `max_calls`는 일일·큐 처리 실행마다, 지식 감사에서는
각 배치마다 적용합니다. 예산이 소진되면 미완료로 중단하고 검증된 작업을 다음 실행에 남깁니다.
모델의 전체 문맥·토큰 수나 요금 한도를 뜻하지는 않습니다. 입력은 최소 4096바이트,
결과는 최소 1024바이트이면서 입력 한도의 1/4 이하여야 하며 호출 수는 양수입니다.
프로필 내부의 `work`는 지정한 항목만 공통 예산에서 덮어씁니다. 예를 들어 작은 문맥의 모델에는
`work: {max_input_bytes: 131072, max_result_bytes: 16384}`를 지정할 수 있습니다.
`lore run ... --json`은 실제 적용한 예산을 출력하고, 자식 명령에도 같은 프로필을 전달합니다.
수동 분할 명령에서 같은 예산을 사용하려면 `LORE_EXECUTION_PROFILE`에 해당 프로필 이름을 지정합니다.
수동 재개·분할 결과 형식은 [분할 근거 처리 절차](skills/lore-process/references/bounded-work.md)를 참고하세요.
실행 파일은 `executable`, 모델은 `model`로 필요한 프로필에만 지정합니다.
상대 실행 파일 경로와 Claude의 `config_directory`는 설정 파일 기준입니다.
Codex의 `profile`은 해당 CLI의 명명된 프로필입니다. 모델 변경만으로 기존 지식을 다시 쓰지 않습니다.

`llm.provider: queue`는 타입이 있는 의미 작업을 보관하는 모드이며 실행기 선택과 별개입니다.
`noop`는 LLM 작업을 만들지 않는 모드입니다. 검색·탐색과 결정적 수집은 에이전트 설치 없이도 사용할 수 있습니다.

---

## 설치 · 빌드

```bash
# 원라인 설치 (macOS / Linux) — 바이너리·템플릿·스킬, SHA256 검증 포함
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.sh | bash

# Windows (PowerShell)
irm https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.ps1 | iex

# 소스에서 빌드
cargo build --release && ./target/release/lore --help
```

설치 플래그: `--version`, `--install-dir`, `--data-dir`, `--skill {user,project,none}`, `--agent {claude-code,codex,both}`, `--from-source`, `--force`, `--yes`, `--dry-run` (`--help`로 전체 확인).

설치기는 설정 디렉터리에 `config.starter.yaml`과 `config.example.yaml`을 배포하며
사용자의 `config.yaml`은 보존합니다. 같은 디렉터리의 `data-dir`은 설치 경로를,
`deployed-skills`는 배포한 스킬을 기록합니다. 업데이트·제거에 쓰는 자동 관리 정보이며
인증정보는 포함하지 않습니다.

### 할 일 관리

```bash
lore agenda                       # 오늘 할 일 — 커밋된 것, 깨어난 것, 기한이 온 것
lore task add "스펙 리뷰" --state today
lore task add "인덱스 질문 회신" --link https://acme.slack.com/archives/C123/p1755600000 --label 스레드
lore task done 7k2p --note "refresh token은 회전 후 재시도하면 grant가 무효화된다"
lore task sync                    # Obsidian에서 직접 고친 것을 기록에 반영
```

보드는 `<personal>/tasks.md` — `오늘 / 제안 / 다음 / 대기 / 언젠가` 섹션의 평범한 마크다운 체크박스입니다. 폰의 Obsidian에서 체크하거나 줄을 다른 섹션으로 끌어 옮기면 다음 `lore task sync`나 보드를 변경하는 명령에서 상태 변경을 기록합니다. 섹션 제목이 곧 상태이며, `제안`은 아직 수락하지 않은 항목입니다.

`lore task done`은 완료와 `--note`를 전환 로그에 저장합니다. 설정된 tasks 소스를 완료일이 포함된 수집 범위로 ingest하면 그날의 데일리 페이지가 생성되고, `/lore-process`가 대기 중인 요약·개념 추출을 처리합니다. 전체 소스 수집에는 work-log도 포함되며, 예약된 일일 파이프라인은 수집과 의미 처리를 함께 실행합니다. **읽은 것뿐 아니라 한 일도 지식의 근거로 남습니다.**

### 업데이트 · 상태 · 제거

```bash
lore self status      # 배포된 사본이 지금 바이너리와 같은지 (다르면 non-zero)
lore self update      # 새 릴리스로 교체하고, 대화형·실행용 스킬과 템플릿·AGENTS.md 재배포
lore self deploy      # 재배포만 (self status가 보고한 차이를 고침)
lore self uninstall   # 설치한 것만 제거 — vault는 건드리지 않음
```

대화형·실행용 스킬과 템플릿·시작 설정·전체 옵션 예제는 **바이너리에 내장**되어 있습니다. 배포된 사본은 변경되거나 뒤처질 수 있으며 `lore self status`로 확인합니다. `lore self deploy`가 사본을 씁니다. `lore self update`는 `lore queue check`와 같은 기준으로 대기 작업·해석 불가 입력·미반영 결과·중단된 저장 기록이 있으면 거부합니다. 확인 가능한 미완료 작업은 `--force`로 허용할 수 있지만, 설정이나 저장소를 읽을 수 없는 오류는 생략하지 않습니다. 실행 중인 버전보다 오래된 릴리스도 거부합니다(`--version`으로 명시하면 되돌릴 수 있음).

---

## 자격증명

환경변수 또는 `<vault>/.lorekeeper/credentials.json`(0600). 환경변수가 파일보다 우선.
macOS·Linux에서는 그룹·다른 사용자에게 권한이 열린 인증 파일과 잠금 파일을 거부합니다.
수동 생성·복원한 파일은 사용 전에 `chmod 600`으로 제한하세요. 저장·갱신은 항상 `0600`으로 수행하며,
인증 설정 마법사는 읽기·파싱에 실패한 기존 저장소를 빈 값으로 초기화하지 않습니다.
여러 vault가 같은 계정을 쓰면 `credentials.file`에 하나의 공유 저장소를 지정할 수 있습니다.
상대 경로는 설정 파일 기준이며, OAuth 갱신과 후속 토큰 저장은 같은 잠금 안에서 처리합니다.
회전하는 refresh token을 별도 파일로 복사하지 마세요. 다른 앱은 별도 OAuth 앱을 사용해야 합니다.

```bash
lore init credentials   # 대화형 마법사 — Google 토큰은 브라우저 OAuth로 자동 발급
```

- **Google**: `LORE_GOOGLE_CLIENT_ID/SECRET/REFRESH_TOKEN` — Gmail/Drive/Calendar **읽기 전용** 스코프. "Desktop app" OAuth 클라이언트 필요.
- **Slack**: `LORE_SLACK_TOKEN`(bot `xoxb-`) 또는 `LORE_SLACK_USER_TOKEN`(`xoxp-`). `slack-search`는 user 토큰 필수.
- **Atlassian** (Jira/Confluence): `LORE_ATLASSIAN_SITE_URL` + `LORE_ATLASSIAN_PAT` (Data Center), or `+ LORE_ATLASSIAN_EMAIL` + `LORE_ATLASSIAN_API_TOKEN` (Cloud 클래식 토큰 — 사이트로 전송). 스코프 토큰이면 `LORE_ATLASSIAN_CLOUD_ID`를 함께 주세요 — 그 값이 게이트웨이 경로를 선택하며, 없으면 클래식으로 취급되어 사이트로 갑니다. OAuth은 리프레시 토큰이 회전하므로 환경변수로 받지 않습니다 — `lore init credentials`로 발급하세요.

> 기본 인증 파일은 Git에서 제외됩니다. 별도로 지정한 공유 저장소도 커밋·동기화 대상에 포함하지 마세요.

---


`lore wiki document-identity plan`에 `--history-ref <전체-커밋-ID>`를 반복 지정하면
과거 원문 사본도 검증합니다. 계획은 Git blob·작성된 ID·정확히 투영된 본문을 고정하고,
`check`와 `apply`가 다시 검증합니다. `apply --allow-blocked`는 검증된 부분만 이관하고
해결되지 않은 페이지는 그대로 둡니다.

현재 인용 근거로 개념 하나를 다시 작성하려면
`lore graph resynthesize wiki/concepts/example.md --dry-run`으로 확인한 뒤
`--dry-run` 없이 요청하고 큐를 처리합니다. 과거 작업이 만료되어도 사용할 수 있으며,
`queue regenerate --task-id`는 원래 입력이 보관된 경우에만 재생합니다.

## 스케줄링

```bash
lore schedule > lorekeeper.cron   # Linux: 검토 후 crontab -e에 병합
lore schedule --format launchd --bin "$(command -v lore)"   # macOS
```

`lore schedule`은 예약 정의를 출력하며 OS에 등록하지 않습니다. Linux에서는 기존 예약을
보존하며 병합하세요. macOS에서는 각 plist를 표시된 LaunchAgents 경로에 저장하고 로그
디렉터리를 만든 뒤 `launchctl bootstrap gui/$(id -u) <plist 절대 경로>`로 등록합니다.
설정 변경 후에는 기존 작업을 `bootout`하고 새 정의를 등록하며, 비활성화한 작업도 해제합니다.
`self uninstall`은 OS 예약을 해제하지 않습니다.

macOS의 문서·데스크탑·다운로드 같은 보호 폴더에 볼트가 있으면 예약 작업의 `lore`에도
해당 폴더 접근 승인이 필요합니다. 터미널 앱의 승인이 launchd 실행까지 보장하지는 않습니다.
작업을 실제로 시작해 표시되는 macOS 권한 요청을 승인하고, 로그와 종료 상태를 확인하세요.
서명 식별자가 달라지는 바이너리 업데이트 후에는 재승인이 필요할 수 있습니다.
파일 권한 `0600`은 이 OS 승인을 대신하지 않습니다.
자세한 권한 동작은 [Apple 문서](https://developer.apple.com/documentation/bundleresources/information-property-list/nsdocumentsfolderusagedescription)를 참고하세요.

예약 작업은 Rust 명령을 직접 실행합니다.

| 예약 | 실행 |
|---|---|
| `ingest.schedule` | `lore run daily --wait` |
| `graph.audit.schedule` | `lore run knowledge --wait` |
| 주간·월간·분기·연간 | `lore synthesis <기간> --previous --defer --wait-for-vault` |
| 유지보수 | `lore maintenance --prune-queue --wait-for-vault` |
| 리마인더 | `lore task remind notify` |

일일 작업은 전체 소스를 수집하고, 큐·역링크·검색 인덱스를 정리한 뒤 기본 기간 → 분기 →
연간 순서로 재개합니다. 일부 소스 실패 후에도 가능한 정리는 진행하며 의존 단계 실패는
그 이후 기간 처리를 중단합니다. 원본 수집과 의미 처리의 성공을 구분해 보고합니다.
`--json`은 단계별 결과를 stdout에, 하위 명령 로그는 stderr에 출력합니다.
에이전트는 선택한 근거·검토를 읽고 의미를 보정하며, 현재 판본을 검증해 결과를 반영합니다.
최종 큐·역링크·검색 갱신과 구조 검사는 Rust 실행기가 맡아 에이전트의 전역 목록 조회와
중복 검사를 줄입니다. 스킬을 수동으로 단독 실행할 때는 스킬의 최종 검사 절차를 따릅니다.
실행 중에는 선택한 설정 경로와 파일 판본을 유지합니다. 설정을 편집하면 남은 작업을
다른 볼트로 넘기지 않고 미완료로 중단합니다. 의도한 설정으로 새 작업을 시작하세요.

의미 처리 동안 Rust 실행기가 볼트의 Unix workflow lease를 유지합니다. 기본 경합은
실패하고 `--wait`는 취소 가능한 대기를 선택합니다. 종료 신호·시간 제한 시 자식 프로세스
그룹을 종료하고 작업을 보존합니다. 짧은 문서·큐 쓰기 잠금과 외부 편집기에 대한 리비전
검증은 별도로 유지합니다. 수동 세션은 입력을 읽기 전에 `lore workflow -- claude` 또는
`lore workflow -- codex`로 시작하세요. descriptor를 닫거나 분리 실행한 작업은 수동 exec
lease의 보장 범위 밖이며, workflow와 무인 실행은 Windows를 지원하지 않습니다.

스케줄러는 설정·런타임 자료의 절대 경로, 고정된 PATH와 필요한 기존 인증 디렉터리를
담습니다. 선택한 호스트의 `CLAUDE_CONFIG_DIR` 또는 `CODEX_HOME`이 명시돼 있으면
유효한 디렉터리인지 확인합니다. 표준 proxy 변수는 대소문자·빈 값을 유지하고,
비밀 값이나 API 키는 예약 정의에 복사하지 않습니다. 환경을 바꾸면 정의를 다시 등록하세요.

예약 시각은 호스트 현지 시간, 문서 날짜는 볼트 시간대입니다. launchd는 잠자기 중 놓친
실행을 깨어난 뒤 한 번 수행하지만 모든 날짜를 backfill하지 않습니다. cron은 놓친 실행을
건너뜁니다. `lore synthesis pending --status --json`으로 수집 범위·의미 처리의 미완료 사유를 확인하세요.
시험 환경은 `--namespace org.example.lore-test --log-dir /absolute/test/logs`로 분리할 수 있습니다.

주간 의미 감사는 수집·기간 합성과 독립적으로 설정합니다.

```yaml
graph:
  audit:
    schedule: "0 7 * * 0"
    batch_size: 5
    max_input_bytes: 262144
```

처음 보는 대상과 변경된 대상을 번갈아 검토하고, 동일 입력은 반복 검토하지 않습니다.
대상은 설정된 위키의 개념과 출처 문서(`type: document`)이며, 문서만 있는 주제도
개념을 만들지 않고 검토합니다.
`kind`는 대상 종류를, `counts_by_kind`는 종류별 전체·현재 검토·대기 건수를 구분합니다.
합계는 두 종류를 모두 포함합니다. 문서의 링크는 문맥이며 자동으로 뒷받침 근거가 되지 않습니다.
`.lorekeeper/knowledge-audit.json`은 최신 검토 판본과 대기 순서를 보관하고 판단·상충 근거는
Markdown 검토 문서에 남깁니다. 단일 출처나 인용 수만으로 병합·삭제하지 않습니다.
초기 누적분은 `lore run knowledge --profile codex --batches 20 --json`처럼 등록된 프로필과
실행할 최대 배치 수를 지정해 처리할 수 있습니다. 프로필 이름은 자신의 설정에 맞춥니다.
각 배치 뒤에는 실행 잠금을 해제하며, 실패·취소 시 중단하고 검증된 검토 기록부터 재개합니다.
`stop_reason: batch-limit`는 지정한 실행량에 도달했다는 뜻이고, `settled`는 현재 감사 대상
계획에 대기 항목이 없다는 뜻입니다. 이 집계가 외부 원본·첨부까지 모두 읽었다거나 전체 지식의
의미적 일관성을 검증했다는 뜻은 아닙니다.
`audit-plan`은 선택한 대상·근거·직접 관련 문맥·검토 문서와 그 근거·선택한 RSS 원본의 중복을
제외한 전체 바이트 수를 보고합니다. `graph.audit.max_input_bytes`는 이 전체 입력의 한도로,
이를 넘으면 `input.status: oversized`를 그대로 표시합니다. 자동 선택도 큰 대상을 건너뛰지
않으며 `--path`는 지정한 범위를 유지합니다. Rust 실행기는 `execution.work` 한도에 맞춰
빠짐없이 나누어 읽고 종합하도록 처리합니다. 전체 입력 한도를 넘는 완료 기록에는 현재 입력에
대한 분할 작업·종합 결과의 완료 증명이 필요합니다. 근거를 자르거나 일부 읽기만으로 완료하지
않으며, 중단된 작업은 보존합니다. `--profile`만으로 어느 바이트 한도도 바뀌지 않습니다.
선택한 대상의 검토가 누락·무효·재평가 필요 상태이면 완료를 거부합니다. 다른 대상의 재평가 대기는
`run knowledge`의 `review_health` 건수에 남겨 보고하며 다음 배치를 막지 않습니다. 무효 기록이나
검사 오류는 실행을 실패시킵니다. 전체 저장소 검사는 `wiki reviews --check`로 별도 수행합니다.
미해결 판단을 남기는 `follow-up`에는 해당 대상을 선언한 현재 `open` 검토가 필요합니다.

리마인더는 OS 알림 명령이 성공한 뒤에만 소진합니다. 이는 OS의 접수이며 사람이 보았다는
확인은 아닙니다. 접수 직후 저장 전에 중단되면 재전달될 수 있습니다. 실패한 알림은 다음 실행에 남습니다.

### 볼트를 git으로 관리하기 (선택)

재합성은 본문을 대체하고 병합은 페이지를 삭제하므로, 중요한 변경 전에는 별도 백업이나 검토한 Git 커밋을 남기십시오. 파이프라인은 사용자의 동시 편집을 구별할 수 없어 자동으로 stage하거나 commit하지 않습니다. 복구 journal은 중단된 변경을 이어가기 위한 기록이며 과거 버전 백업을 대신하지 않습니다.

```bash
cd "$(lore config vault-root)"
git init
git config core.precomposeunicode true        # macOS: 한글 파일명을 NFC로 저장
# 기존 .gitignore에 아래 항목을 추가하고 중복·사용자 설정을 확인
printf '.lorekeeper/\n.obsidian/\n' >> .gitignore
git status --short
git diff
# 공개해도 되는 파일만 선택해서 git add한 뒤 commit
```

`.lorekeeper/`에는 운영 기록과 보관된 입력이 있으므로 이력에서 제외하십시오. 자격증명 파일도 별도로 제외되어 있는지 확인하십시오. 작은 진입 안내인 `wiki/index.md`는 재생성할 수 있으며, 경로는 `vault.dirs.wiki` 설정에 맞춰 제외할 수 있습니다. 원격 저장소를 쓴다면 원문 데이터의 공유 범위를 먼저 확인하십시오.

---

### 소스 확장과 수집 범위

소스별 어댑터가 수집·인증·관측 범위를 담당하고, 공통 파이프라인이 정규화·개념 추출·합성·관계·검토를 처리합니다.
새 소스는 타입·설정 검증·문서 ID·부분 실패 계약을 명시적으로 등록합니다. 설정에 이름만 추가하는 플러그인 방식은 아닙니다.

Drive는 `folder_id`와 선택적 `name_prefix`를 사용하고 실제 수정일로 조회합니다. 파일 ID는 제목·폴더와 분리되어 유지됩니다.
Google Docs는 Markdown, Slides는 텍스트로 가져오며 시각 자료와 지원하지 않는 파일 형식은 완전한 수집으로 간주하지 않습니다.
조회 제한이나 읽기 중 판본 변경은 부분 관측으로 보고합니다. 수정일 조회는 과거 전체 상태 복원이나 원격 삭제 탐지가 아니며,
이번 결과에 없는 기존 문서는 보존합니다. 수신 데이터 해시는 과거 원본을 복원하는 저장소를 대신하지 않습니다.

현재 YouTube 전용 수집기는 없습니다. 향후 추가할 때 영상·자막 트랙·언어·구간의 식별과 취득 권한을 정의해야 합니다.
웹 URL 입력도 임의 웹사이트의 지속 동기화나 동적 페이지 전체 보존을 보장하지 않습니다.
비스트리밍 일일 소스는 부분 수집 결과로 기존의 완전한 하루를 덮어쓰지 않고 실패로 보고합니다.
독립 문서와 누적 스트리밍 소스는 성공한 항목을 각각 보존합니다.

## 더 알아보기

- **설정 전체 레퍼런스** — [`config.example.yaml`](config.example.yaml) (모든 소스·옵션 주석 포함)
- **페이지 포맷 스키마** — `lore schema`로 vault에 `wiki/AGENTS.md` 생성
- **아키텍처 깊이 보기** — [DeepWiki](https://deepwiki.com/junyeong-ai/lorekeeper)

## 기술 스택

Rust 2024 edition · 빌드 1.99.1 · 최소 지원 1.97. `tokio`/`reqwest`(비동기 소스), `jiff`(타임존 정확 날짜), `serde-saphyr`(YAML), `minijinja`(템플릿), `feed-rs`(피드), `petgraph`(링크 그래프), `blake3`(이벤트·캐시 해시). 라이선스: MIT.
