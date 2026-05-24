# Google (Calendar / Gmail) 설정값 조회 (`gws`)

> `gws`는 출력을 stdout(JSON), 진단을 stderr로 낸다 — 파싱 시 `2>/dev/null`.

## Calendar — 캘린더 id
```bash
gws calendar calendarList list 2>/dev/null
```
보통 `primary`(본인 기본 캘린더). 공유/팀 캘린더는 그 id를 쓴다.
```yaml
my-schedule:
  type: google-calendar
  enabled: true
  schedule: "0 8 * * 1-5"
  params: {calendar_id: primary, lookback_hours: 24, lookahead_hours: 24}
  labels: [personal]
  extract_concepts: false
  track_personal: true
```
일정 description은 HTML→Markdown으로 자동 변환된다.

## Gmail — 어떤 메일을 수집할지
안 읽은 메일의 카테고리 분포를 보고 노이즈를 가늠:
```bash
for c in primary promotions updates social forums; do
  n=$(gws gmail users messages list --params "{\"userId\":\"me\",\"q\":\"is:unread category:$c\",\"maxResults\":1}" 2>/dev/null \
       | python3 -c "import sys,json;print(json.load(sys.stdin).get('resultSizeEstimate','?'))" 2>/dev/null)
  echo "$c: $n"
done
```
대개 `category:primary`만 업무 메일이고 나머지는 봇/마케팅. 권장:
```yaml
email-digest:
  type: gmail
  enabled: true
  schedule: "30 8 * * 1-5"
  params:
    lookback_hours: 24
    include_queries: ["category:primary"]   # 봇/알림(GitHub 등) 제외, 업무 메일만
  classify:                                   # 선택: 키워드→분류 섹션
    action_required: ["검토 요청", "확인 부탁", "please review"]
    decisions: ["승인", "결재 완료", "approved"]
  labels: [personal]
  extract_concepts: true
  track_personal: true
```

## refresh token이 없을 때
`lore init credentials`로 브라우저 OAuth 발급(Desktop-app 클라이언트, 읽기 전용 스코프).
