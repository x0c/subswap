# subswap - Claude, Codex, ChatGPT 계정 전환 도구

Languages: [English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | 한국어

subswap은 Claude Code, Anthropic Claude, OpenAI Codex CLI, ChatGPT의 여러 AI 구독 계정을 관리하는 Rust CLI입니다. 로컬 로그인 상태를 가져오고, 자격 증명을 파일에 저장하며, quota window를 확인하고, 사용량이 설정된 threshold를 넘으면 활성 계정을 수동 또는 자동으로 전환합니다.

Claude 계정 전환 도구, Codex 계정 관리자, ChatGPT quota tracker, 또는 여러 Provider를 통합하는 구독 전환 도구로 사용할 수 있습니다.

**플랫폼 지원**: macOS 및 Linux에서 테스트되어 정상 동작합니다. Windows는 미테스트입니다.

## 기능

- **Claude Code와 Codex CLI의 다중 계정 전환**: 다시 로그인하지 않고 활성 계정을 바꿉니다.
- **Claude Code 커스텀 API 엔드포인트**: 인터랙티브 위저드로 DeepSeek 등 Anthropic 호환 엔드포인트를 추가하고 일반 Claude 계정처럼 전환할 수 있습니다.
- **계정 격리 병렬 환경**: `subswap run`·`shell`·`env`로 글로벌 활성 계정을 변경하지 않고 서로 다른 터미널에서 각기 다른 계정을 동시에 사용할 수 있습니다. 자격 증명은 전용 디렉터리에 투영되고 네이티브 CLI는 환경 변수로 유도됩니다.
- **Quota-aware status**: 사용 가능한 경우 Claude 5h / 7d 사용량, Codex / ChatGPT 사용 데이터 같은 Provider quota window를 표시합니다.
- **자동 계정 전환**: 백그라운드 daemon이 사용량이 threshold를 초과한 계정에서 전환하고, 매 quota 업데이트 시 재판정하여 항상 최선의 계정을 선택합니다.
- **자동 전환 토글**: `subswap autoswap on/off`로 설정 파일을 건드리지 않고 자동 전환을 켜거나 끌 수 있습니다.
- **수동 전환 후 정착 유예**: 수동으로 계정을 선택한 후 daemon은 유예 기간 동안 자동 전환을 보류하여 의도가 즉시 덮어써지지 않도록 합니다.
- **네트워크에 의존하지 않는 수동 전환**: quota API 실패, token 만료, 네트워크 장애가 있어도 `subswap swap`은 동작합니다.
- **Quota 결과 캐시와 stale fallback**: 백그라운드 갱신 중에도 캐시 결과를 반환하여 상태 화면이 항상 응답합니다.
- **파일 기반 자격 증명 저장**: 토큰은 앱 데이터 디렉터리 내 소유자만 읽을 수 있는 `0600` 파일에 보관됩니다. 기존 keyring 기반 설치는 첫 실행 시 자동 마이그레이션됩니다.
- **Provider 기반 아키텍처**: Claude / Anthropic과 Codex / ChatGPT는 별도 crate이므로 core 정책을 바꾸지 않고 새 AI Provider를 추가할 수 있습니다.

## 지원 클라이언트

| Provider | 로컬 클라이언트 | subswap이 관리하는 항목 |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth 자격 증명, 커스텀 API 엔드포인트, 활성 계정 파일, 5h / 7d quota, token keepalive |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json` passthrough, 활성 계정 파일, ChatGPT 사용량 조회 |

## 일반적인 사용 사례

- 여러 Claude Pro, Claude Max, ChatGPT Plus, ChatGPT Team seat 사이를 전환합니다.
- 현재 계정이 사용 한도에 도달했을 때 사용할 예비 AI 구독을 준비해 둡니다.
- 서로 다른 터미널에서 두 계정을 동시에 간섭 없이 사용합니다.
- 긴 코딩 세션을 시작하기 전에 계정별 사용량을 확인합니다.
- Claude와 ChatGPT 계정 전환을 하나의 CLI로 통합합니다.

## 상태

| Milestone | Scope | State |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: keyring-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json passthrough, atomic write, tolerant wham/usage parsing | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |
| M5 | 계정 격리 실행 환경, 자동 전환 토글, quota 캐시, 정착 유예 | done |

## 왜 필요한가

여러 AI 구독을 사용한다면 다음 상황을 겪을 수 있습니다.

- Claude Pro 사용량을 다 써서 다시 로그인하지 않고 ChatGPT로 전환하고 싶다.
- ChatGPT seat 두 개를 보유하고 있고, 한 줄 명령으로 활성 계정을 바꾸고 싶다.
- 두 계정을 서로 다른 터미널에서 간섭 없이 동시에 쓰고 싶다.
- 계정별 window(5h / 7d)에 남은 사용량을 보고 싶다.

subswap은 각 계정의 자격 증명을 앱 데이터 디렉터리 내 소유자 전용 파일에 저장하고(첫 실행 시 기존 OS keyring 항목 자동 마이그레이션), 같은 온디스크 자격 증명 파일을 읽는 모든 클라이언트에서 활성 계정을 원자적으로 전환합니다. 수동 전환은 네트워크 때문에 막히지 않으며, quota 조회는 참고 정보로만 사용됩니다.

## 설치

Homebrew 사용:

```bash
brew install x0c/tap/subswap
```

먼저 tap을 추가한 뒤 이름으로 설치할 수도 있습니다.

```bash
brew tap x0c/tap
brew install subswap
```

소스에서 설치하려면 Rust 1.80+가 필요합니다.

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

Git에서 직접 설치할 수도 있습니다.

```bash
cargo install --git https://github.com/x0c/subswap --path crates/cli
```

## 빠른 시작

```bash
# default: sync local active accounts, fetch quotas, auto-swap if past threshold,
# then print a one-screen status. Run this whenever you want to know what's up.
subswap

# manually swap to a specific account (escape hatch — never depends on the network)
subswap swap alice@example.com
# disambiguate when the same id exists under multiple providers:
subswap swap claude/alice@example.com

# 인터랙티브하게 DeepSeek 등 Claude Code 호환 API 추가
subswap add-api
# 커스텀 API 엔드포인트는 수동 전용 — 자동 전환에 참여하지 않음
subswap swap deepseek

# 글로벌 활성 계정을 변경하지 않고 격리 환경에서 계정 사용
subswap run codex bob@example.com -- --version   # bob 계정으로 codex 격리 실행
subswap shell alice@example.com                  # alice 계정으로 격리된 서브 셸 열기
eval "$(subswap env codex/bob@example.com)"      # 현재 셸을 임시로 codex 계정에 지정

# 자동 전환 활성화 / 비활성화
subswap autoswap on
subswap autoswap off

# registry와 keyring에서 계정 삭제
subswap rm alice@example.com

# 환경 자가 진단 (클라이언트 파일, keyring, 설정 디렉터리)
subswap doctor
```

Claude Code / Codex CLI에 최소 한 번 로그인했다면, 처음 `subswap`을 실행할 때 `~/.claude`와 `~/.codex`에서 계정을 자동으로 가져옵니다.

첫 `subswap` 실행은 macOS가 아닌 Unix 플랫폼에서 분리된 백그라운드 daemon(`subswapd`)도 시작합니다. 이 daemon은 quota를 폴링하고 백그라운드에서 자동 전환을 수행하며, Claude OAuth token을 최신 상태로 유지해 오래 쉬던 계정으로 전환하는 순간 401이 발생하는 일을 줄입니다. macOS에서는 분리된 프로세스의 Keychain 접근이 추가 인증 프롬프트를 만들 수 있으므로 명시적인 opt-in이 필요합니다. 자동 시작을 켜려면 `SUBSWAP_AUTO_DAEMON=1`을 export하세요. daemon은 단일 인스턴스(파일 잠금)입니다. 종료해도 안전합니다: `pkill -f 'subswap __daemon'` 또는 `pkill subswapd`. 완전히 비활성화하려면 `SUBSWAP_NO_DAEMON=1`을 export하세요.

## 계정 격리 환경

`subswap run / shell / env`를 사용하면 글로벌 활성 계정을 변경하지 않고 여러 터미널에서 계정을 병렬로 사용할 수 있습니다. 자격 증명은 전용 디렉터리에 투영되고 네이티브 CLI는 환경 변수로 유도됩니다(Codex는 `CODEX_HOME`; macOS의 Claude는 `CLAUDE_CONFIG_DIR`와 `CLAUDE_SECURESTORAGE_CONFIG_DIR`).

```bash
subswap run codex 6 -- --version       # 계정 #6으로 격리하여 codex 실행
subswap run claude alice@x.com         # alice 계정으로 claude 격리 실행
subswap shell 3                         # 계정 #3으로 격리된 서브 셸 진입
eval "$(subswap env codex/bob@x.com)"  # 현재 셸을 임시로 codex 계정에 지정
```

- **배타적 잠금**: 계정당 하나의 격리 세션만 허용 — refresh token 순환 충돌 방지.
- **글로벌 활성 경고**: 현재 글로벌 활성 계정으로 격리 세션을 시작하면 경고가 표시됩니다 — 비격리 클라이언트가 동시에 사용 중이면 refresh token이 무효화될 수 있습니다.

## 설계 불변 조건

기여 전에 알아둘 핵심 전제입니다.

1. **`swap`은 quota 조회에 의존하지 않습니다.** API가 내려갔거나, keyring에 접근할 수 없거나, token이 만료되어도 수동 전환은 활성 계정 변경을 시도해야 합니다.
2. **Secret은 레지스트리 메타데이터에 포함되지 않으며 스냅샷은 소유자만 읽을 수 있습니다.** OAuth/API 시크릿은 소유자 전용 자격 증명 저장소에 보관됩니다. 커스텀 API가 활성화된 동안에는 Claude Code도 `~/.claude/settings.json`에 API 키가 필요합니다. subswap은 해당 파일을 원자적으로 보존하고 OAuth로 돌아올 때 복원합니다.
3. **전환은 원자적이며 rollback 가능합니다.** 각 `activate`는 무엇이든 수정하기 전에 `state_dir/snapshots/<ts>/` 아래에 snapshot을 씁니다. 쓰기 하나라도 실패하면 rollback합니다.
4. **Provider 추가 = `crates/providers/<id>` crate 추가 + `cli/src/main.rs::AppContext::build()`에 한 줄 등록.** `core`에는 Provider별 로직을 넣지 않습니다.
5. **Auto-swap threshold는 중앙에서 관리되고 설정 가능합니다.** 컴파일된 기본값은 `crates/core/src/defaults.rs`에 있으며, runtime config로 덮어쓸 수 있습니다.

자세한 내용: [`docs/`](docs/) (중국어 내부 협업 문서).

## 비교

| 도구 | 초점 | 차이점 |
|---|---|---|
| 단일 Provider 계정 전환 도구 | 한 번에 하나의 upstream | subswap은 하나의 Provider 추상화 아래에서 Claude와 Codex / ChatGPT를 지원 |
| quota dashboard | 사용량 표시만 제공 | subswap은 quota window가 가득 찼을 때 다른 로컬 계정을 활성화할 수도 있음 |
| 수동 로그인/로그아웃 | 한 번에 한 계정 | subswap은 등록 계정을 보관하고 활성 로컬 파일을 원자적으로 전환 |

## FAQ

### `subswap swap`은 quota API를 호출하나요?

아니요. 수동 전환은 escape hatch이며 quota 조회에 의존하지 않습니다. upstream API가 내려갔거나 token이 만료되어도 `subswap swap claude/alice@example.com`은 해당 로컬 계정 활성화를 시도합니다.

### token은 어디에 저장되나요?

token과 refresh token은 앱 데이터 디렉터리 내 소유자 전용 자격 증명 파일에 저장됩니다. 커스텀 API가 활성화된 동안에는 Claude Code도 `~/.claude/settings.json`에 API 키가 필요합니다. 전환 스냅샷도 동일하게 `0600`으로 제한됩니다.

### 커스텀 API는 자동 전환에 참여하나요?

아니요. 커스텀 API는 `manual_only`입니다. subswap이 자동으로 선택하지 않으며, 활성화된 동안에는 자동 전환도 완전히 비활성화됩니다. OAuth 계정으로 수동 전환하면 API 모드 진입 전의 Claude Code 설정이 복원됩니다.

### Claude 전용인가요?

아니요. 처음 지원하는 Provider는 Claude / Anthropic과 Codex / ChatGPT입니다. core crate가 Provider trait를 제공하므로, 향후 AI 구독 Provider를 별도 crate로 추가할 수 있습니다.

### Windows에서 동작하나요?

Windows에서는 테스트되지 않았습니다. macOS와 Linux가 주요 지원 플랫폼입니다.

## GitHub topics

공개 후 추천하는 repository topics:

`claude-code`, `codex-cli`, `chatgpt`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `keyring`, `automation`

## 레이아웃

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
```

## 기여

Issues와 PR을 환영합니다. 참고:

- `docs/`와 `AGENTS.md`의 내부 문서는 중국어입니다. 코드 주석은 중국어입니다. 사용자가 보는 모든 내용(CLI 텍스트, 오류 메시지, tracing 로그, crate description)은 영어입니다.
- PR을 열기 전에 `cargo check --workspace`와 `cargo test --workspace`를 실행하세요.

## License

MIT — [`LICENSE`](LICENSE)를 참고하세요.
