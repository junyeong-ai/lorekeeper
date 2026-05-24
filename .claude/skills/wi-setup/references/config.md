# config.yaml 스키마

전체 예시는 repo 루트의 `config.example.yaml`을 기준으로 한다(소스 타입별 블록 + 주석
포함). 여기서는 소스 블록을 둘러싸는 상위 구조만 요약한다.

```yaml
vault:
  root: ~/Documents/Obsidian Vault   # 상대경로는 config 파일 기준
  timezone: Asia/Seoul               # IANA 또는 system
  locale: ko                         # 라벨 언어: ko | en (본문은 원본 유지)

identity:                            # personal 분리·성과 추적의 기준
  name: "..."
  email: "...@company.com"           # Calendar 참석자/Jira assignee 매칭
  slack_id: "U0..."                  # 내 Slack 메시지 → work-log
  jira_id: ""

sources:                             # 키 = 소스 id = daily/{id}/ 디렉토리명
  <id>:
    type: gmail | google-calendar | google-drive | slack-channel | slack-search | jira
    enabled: true
    schedule: "0 9 * * 1-5"          # cron (wi schedule이 사용)
    params: { ... }                  # 타입별 (각 references 참고)
    classify: { category: [keywords] }   # 선택, gmail 등
    labels: [ ... ]
    extract_concepts: true|false     # LLM 개념 추출 여부
    track_personal: true|false       # work-log 집계 대상

dedup: {cascade: [event-id, url, title], title_threshold: 0.85}
labels: {categories: [...]}
performance: {...}                   # 성과 카테고리 매핑 (config.example 참고)
synthesis: {weekly: {...}, monthly: {...}, quarterly: {...}, annual: {...}}
llm: {provider: queue, model: claude-sonnet-4-6, max_tokens: 4096}
```

## 위치 / 검증
- `./config.yaml`(repo) 또는 `~/.config/wi-ingest/config.yaml`(바이너리 설치).
  `--config`/`WI_CONFIG`로도 지정. vault 하위에는 둘 수 없다(vault 경로가 config 안에 있어 순환).
- 작성 후 항상 `wi validate` → `wi ingest --dry-run`으로 확인.
- 기존 config가 있으면 통째로 덮어쓰지 말고 해당 소스 블록만 병합한다.
