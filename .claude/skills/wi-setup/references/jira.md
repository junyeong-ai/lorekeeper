# Jira 설정값 조회 (`atlassian-cli`)

> 주의: `atlassian-cli`는 자체 OAuth 계정을 쓴다. wiki-ingest는 `credentials.json`의
> Jira 계정(email+token)을 쓰므로, 둘이 다른 인스턴스면 결과가 다를 수 있다. 최종
> 확인은 `wi ingest --dry-run`으로 wiki-ingest 계정 기준 검증한다.

## 내 이슈 / 프로젝트 키
```bash
atlassian-cli jira search "assignee = currentUser() ORDER BY updated DESC" \
  --limit 5 --fields summary,project,duedate
```
`project.key`(예: `OYAI`)를 JQL에 쓴다.

## 시작일 custom-field id 찾기
Jira의 "Start date"는 인스턴스별 custom-field(흔히 `customfield_10015`). 한 이슈를
받아 날짜 필드를 확인:
```bash
atlassian-cli jira get <ISSUE-KEY> --fields "*all" | grep -i -A2 "start date"
```
찾은 id를 `start_date_field`에 넣는다. 없으면 생략(시작일 표시 안 함).

## config 블록
```yaml
my-tasks:
  type: jira
  enabled: true
  schedule: "0 9 * * 1-5"
  params:
    # 그날 변경된(=작업한) 이슈만 = 작업 이력 스냅샷. 마감/시작일로 검색하지 않는다.
    jql: >
      project = OYAI AND assignee = currentUser()
      AND updated >= -1d
      ORDER BY updated DESC
    max_results: 50
    start_date_field: customfield_10015   # 선택: 시작일 표시
  labels: [personal]
  extract_concepts: false
  track_personal: true
```
description은 ADF→Markdown으로, 상태·기간은 그날 시점 스냅샷 헤더로 자동 렌더된다.
