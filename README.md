# mykakao

macOS 카카오톡의 로컬 메시지 DB를 **"대화 내보내기" 없이** 복호화해서, 브라우저에서 대화방·메시지·검색·**실시간 스트림**을 확인하는 데모.

> 💡 복호화는 **표준 `sqlcipher` + 자체 키 유도**로 직접 한다. 외부 런타임 의존 없음.
> 🔒 **본인 기기에서 본인 카톡 데이터에만** 동작한다. 키는 이 기기의 device UUID + 본인 user_id 에서 유도되므로 다른 사람의 DB는 못 푼다.

---

## 어떻게 동작하나

카톡 메시지 DB는 SQLCipher로 암호화돼 있다. 복호화 키는 `device UUID` + `user_id` 두 값에서 결정적으로 유도된다. UUID는 `ioreg`로, user_id는 plist에 박힌 `SHA512(user_id)` 해시를 역추적(brute-force)해 복구한다.

```
ioreg (device UUID) ─┐
plist (user_id 복구: SHA512 preimage) ─┼─▶ 키 유도(PBKDF2) ─▶ sqlcipher 복호화 ─▶ SQLAlchemy ─▶ FastAPI ─▶ 웹 UI
컨테이너 DB 파일(78-hex) ─┘                                                                         └─▶ SSE 실시간 스트림
```

| 파일 | 역할 |
|---|---|
| `backend/extract.py` | device UUID + user_id 복구 + 키 유도 (전부 자체 구현) |
| `backend/db.py` | `sqlcipher3` 드라이버에 키 주입한 SQLAlchemy 엔진 (`mode=ro`, NullPool) |
| `backend/models.py` | 카톡 테이블 ORM 매핑 (NTChatRoom / NTChatMessage / NTUser) |
| `backend/main.py` | FastAPI — REST 4종 + SSE + 프론트 서빙 |
| `frontend/index.html` | 바닐라 JS 단일 페이지 (사이드바·말풍선·검색·LIVE) |
| `run.sh` | venv 생성 → 의존성 설치 → 키 복구 → 서버 기동 |

---

## 사전 준비 (각자 자기 맥에서)

1. **카카오톡 데스크톱 앱에 로그인**되어 있어야 한다 (로컬 DB가 있어야 함).
2. **Homebrew + sqlcipher**
   ```bash
   brew install sqlcipher
   ```
3. **Xcode Command Line Tools** — `sqlcipher3` 와 user_id 복구(C 가속) 빌드에 필요.
   ```bash
   xcode-select --install   # 이미 있으면 무시됨
   ```
4. **터미널에 전체 디스크 접근(Full Disk Access) 권한**
   시스템 설정 → 개인정보 보호 및 보안 → 전체 디스크 접근 → 사용하는 터미널 앱(Terminal/iTerm 등) 추가 → 터미널 재시작.
   (없으면 보호된 컨테이너 안의 DB 파일을 못 읽는다.)

---

## 실행

```bash
git clone https://github.com/kknaksss/mykakao.git
cd mykakao
./run.sh
```

→ 브라우저에서 **http://127.0.0.1:8765**

- 첫 실행은 venv 생성 + `sqlcipher3` 소스 빌드로 1~2분 걸린다.
- user_id 는 한 번 복구해 `config.json`(gitignore)에 캐시한다 → 다음 실행부터 즉시.
- 포트 변경: `PORT=9000 ./run.sh`

---

## 기능

| 화면 | 설명 |
|---|---|
| 상단 배지 | 총 메시지 수 / 대화방 수 / user_id / DB 파일 |
| 좌측 사이드바 | 최근 활동순 대화방 목록 |
| 메시지 뷰 | 방 선택 시 말풍선(내 메시지 우측 노랑) |
| 검색 | 전체 메시지 키워드 검색(한국어 OK, 하이라이트) |
| `● LIVE` | 카톡에 새 메시지가 오면 ~1초 내 자동 반영(SSE). 열어둔 방이면 말풍선, 아니면 우하단 토스트 |

### API

| Method | Path | 설명 |
|---|---|---|
| GET | `/api/stats` | 총량/메타 |
| GET | `/api/chats?limit=` | 최근 대화방 |
| GET | `/api/messages?chat_id=&limit=` | 특정 방 메시지 |
| GET | `/api/search?q=&limit=` | 키워드 검색 |
| GET | `/api/stream?after=` | SSE 실시간 스트림(새 logId push) |

---

## 트러블슈팅

| 증상 | 원인 / 해결 |
|---|---|
| `user_id 복구 실패` | 카톡 로그인 상태 + Full Disk Access 확인. 권한 준 뒤 터미널 재시작. |
| 사이드바/메시지 빈 화면, 권한 에러 | Full Disk Access 미부여 → DB 파일 읽기 실패. |
| `sqlcipher 가 없다` | `brew install sqlcipher` |
| `sqlcipher3` 빌드 실패 | Xcode CLT 설치(`xcode-select --install`). run.sh가 brew sqlcipher 경로를 잡아준다. |
| 새 메시지가 안 보임 | `mode=ro`로 열어 WAL을 읽는다(코드 반영됨). `immutable`로 열면 스냅샷 고정되어 안 보인다. |
| 포트 충돌 | `PORT=9000 ./run.sh` |

---

## 동작/한계 메모

- **읽기 전용**(`mode=ro`)으로만 연다 — 카톡 DB를 절대 수정하지 않는다.
- 실시간은 진짜 write 후킹이 아니라 DB **1초 폴링** 기반(~1초 지연).
- 오픈채팅(대형) 방은 발신자/방 이름이 로컬 친구목록(NTUser)에 없어 `(이름 없음)`으로 표시된다. 1:1·친구 방은 정상.
- `chatId`/`logId`는 18자리(>2^53)라 API에서 문자열로 주고받는다.

---

## ⚠️ 주의

- **개인용/로컬 전용.** 각자 자기 맥에서 자기 카톡 데이터에만 동작한다.
- `config.json`에 본인 user_id가 들어가므로 **커밋하지 않는다**(이미 `.gitignore`).
- 복호화 키는 메모리에서 유도되며 어디에도 저장하지 않는다.

## 범위

이 데모는 **메시지 추출 확인**까지다. (일정 파싱/캘린더 연동은 다음 단계)
