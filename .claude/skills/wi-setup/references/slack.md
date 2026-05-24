# Slack 설정값 조회 (`slack-cli`)

## 채널 ID 찾기
```bash
slack-cli channels "<이름 일부>" --expand name,id,members -j
```
`id`(예: `C0A7G194EH0`)를 `channels`에 넣는다. 이름(`#name`)도 되지만 id가 안정적
(rename에 영향 없음). 여러 채널은 부분검색을 반복.

## 내 user id (personal 분리용)
```bash
slack-cli auth status            # workspace 확인
slack-cli users "<내 이름>" -j   # id 예: U0AN3404QB0 → identity.slack_id
```

## 채널 성격 파악 (선택)
```bash
slack-cli messages <CHANNEL_ID> --limit 30 --exclude-bots --expand reply_users_count,user_name -j
```
쓰레드 비율이 높으면 `include_threads: true`가 중요(답글에 실제 논의가 있다).

## config 블록
```yaml
team-slack:
  type: slack-channel
  enabled: true
  schedule: "0 9 * * 1-5"
  params:
    channels: ["C0A7G194EH0", "C0A8A0XC5BJ"]   # 팀 채널 (전체 = 팀 현황)
    lookback_hours: 24
    include_threads: true     # 쓰레드 답글 맥락 포함
    exclude_bots: true        # 봇/통합 제외 (기본 true)
    # watch_users: ["U0AN3404QB0"]   # 특정인 작성/멘션만; 비우면 채널 전체
  labels: [team-ops, personal]
  extract_concepts: true
  track_personal: true        # identity.slack_id가 쓴 메시지 → work-log로 분리
```
키워드 트렌드가 필요하면 `slack-search` 타입(아래)을 별도 소스로:
```yaml
  type: slack-search   # user token(xoxp) 필요 — search.messages는 봇 토큰 불가
  params:
    queries:
      - {channel: "#ai-general", keywords: [AI, LLM, RAG]}
    lookback_hours: 24
```
