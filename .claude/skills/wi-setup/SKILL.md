---
name: wi-setup
description: Build or edit a wiki-ingest config.yaml by inspecting the user's real workspace — discovers Slack channel IDs, Jira projects and custom-field IDs, Google calendars, and Gmail query categories via their CLIs, then writes a validated config. Use when the user wants help setting up wiki-ingest, adding a source, or finding the concrete IDs a source needs. Read-only against the user's accounts; only writes config.yaml after confirmation.
when_to_use: |
  설정 자동화, config 만들어줘, wiki-ingest 설정, 소스 추가, 채널 ID 찾아줘,
  프로젝트 찾아줘, 캘린더 설정, setup wiki-ingest, configure sources,
  find channel id, find jira project, build my config
---

# wi-setup — wiki-ingest 설정 자동화

사용자의 실제 워크스페이스를 조회해 구체적 ID/조건을 채운 `config.yaml`을 만든다.
손으로 채널 ID·프로젝트·custom-field를 찾을 필요를 없앤다.

## Workflow

1. **소스 선택** — Gmail / Calendar / Jira / Slack 중 무엇을 수집할지 확인.
   모르면, 각자 무엇을 남기는지 한 줄로 설명하고 추천한다.
2. **실제 값 조회** — 선택한 소스에 해당하는 참조 파일만 읽고, 거기 명령으로
   ID/조건을 조회한다 (account별 인증은 이미 돼 있다고 가정; 안 되면 사용자에게
   해당 CLI 로그인을 안내):
   - Slack → `references/slack.md`
   - Jira → `references/jira.md`
   - Google (Calendar/Gmail) → `references/google.md`
3. **config 작성** — `references/config.md`의 스키마로 작성. 위치는
   `./config.yaml`(repo) 또는 `~/.config/wi-ingest/config.yaml`(바이너리 설치).
   기존 파일이 있으면 덮어쓰지 말고 해당 소스 블록만 병합.
4. **검증** — `wi validate`로 확인하고, `wi ingest --dry-run`으로 인증·추출을 미리 본다.

## 설계 원칙 (설정 시 반드시 반영)

- **데이터는 원본 언어 보존** — 라벨 언어만 `vault.locale: ko|en`으로 선택.
- **Jira는 `updated` 기준** — 그날 작업한 이슈 = 스냅샷. 마감/시작일은 표시용
  (`start_date_field`는 인스턴스별 custom-field id). 날짜로 검색하지 않는다.
- **Slack은 채널 전체** = 팀 현황. 내 메시지는 `track_personal: true` +
  `identity.slack_id`로 work-log에 자동 분리. `watch_users`는 특정인만 좁힐 때만.
- **Gmail은 `category:primary`** 권장 — 봇/알림(GitHub 등)을 빼고 업무 메일만.
- 자격증명은 `wi init credentials`로 입력 — 이 스킬은 ID/조건만 채운다.
