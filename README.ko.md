<p align="center">
  <a href="README.md"><img src="https://img.shields.io/badge/English-gray" alt="English"></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-gray" alt="简体中文"></a>
  <a href="README.ja.md"><img src="https://img.shields.io/badge/%E6%97%A5%E6%9C%AC%E8%AA%9E-gray" alt="日本語"></a>
  <a href="README.ko.md"><img src="https://img.shields.io/badge/%ED%95%9C%EA%B5%AD%EC%96%B4-%E2%9C%93-blue" alt="한국어"></a>
</p>

<h1 align="center">subswap</h1>

<p align="center"><strong>AI 코딩 도구의 남은 사용량을 확인하고 계정을 전환하세요.</strong></p>

<p align="center">Claude Code, Codex, Kimi Code, Cursor, OpenCode, Command Code의 로그인을 저장하고 관리합니다. 사용량이 남은 계정을 확인하고 명령 하나로 전환하세요.</p>

<p align="center">
  <a href="https://github.com/x0c/subswap/actions/workflows/ci.yml"><img src="https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/x0c/subswap/actions/workflows/release.yml"><img src="https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/x0c/subswap" alt="License"></a>
</p>

<p align="center">
  <img src="docs/images/demo-swap.gif" width="920" alt="터미널 데모: 계정 목록을 보고 subswap swap으로 다른 Codex 계정 선택">
</p>
<p align="center"><em>예시 계정을 사용한 데모입니다.</em></p>

## 설치

**Homebrew가 설치된 macOS 또는 Linux**:

Linux 사전 빌드 패키지는 glibc 2.39 이상이 필요합니다. 이전 시스템에서는 소스에서 빌드하세요.

```bash
brew install x0c/tap/subswap
subswap --help
```

<details>
<summary>Windows / 사전 빌드 다운로드 / 소스에서 설치</summary>

**Windows**

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
subswap --help
```

설치 프로그램은 최신 Windows Release를 다운로드하고 SHA-256을 검증한 뒤 `subswap.exe`를 사용자 `PATH`에 추가합니다. [최신 Release](https://github.com/x0c/subswap/releases/latest)에서 zip과 체크섬을 직접 받을 수도 있습니다.

**사전 빌드 다운로드**

[최신 Release](https://github.com/x0c/subswap/releases/latest)는 macOS(Apple silicon / Intel), Linux(ARM64 / x64), Windows(x64) 패키지를 제공합니다. 설치 전에 함께 제공되는 SHA-256을 확인하세요.

**소스에서 설치** (현재 안정판 Rust, 개발용)

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --locked --path crates/cli
subswap --help
```

일반 사용에는 Homebrew 또는 Release 패키지를 권장합니다.

</details>

## 처음 사용하기

먼저 지원하는 네이티브 클라이언트를 설치하고 로그인하세요. 자동 전환은 기본적으로 켜져 있습니다. 직접 계정을 선택하려면 수동 모드로 시작하세요.

```bash
subswap autoswap off
subswap                # 로컬 로그인을 저장하고 계정과 사용량 표시
```

계정 간 전환 전에 다른 계정도 저장하세요. Claude Code와 Codex는 subswap에서 네이티브 로그인 절차를 시작할 수 있습니다.

```bash
subswap login codex    # 다른 Codex 계정으로 로그인. Claude Code는 claude 지정
subswap swap 2         # 2를 계정 목록에 표시된 번호로 변경
```

Kimi Code, Cursor, Command Code는 네이티브 클라이언트에서 다른 계정으로 로그인한 뒤 `subswap login kimi`, `subswap login cursor`, `subswap login commandcode`로 저장합니다. `subswap login opencode`는 공식 Console 계정을 가져오며 필요하면 공식 로그인을 시작합니다. `subswap login opencode-api-key`는 Go API key를 별도 목록으로 가져옵니다.

Codex 전환 후에는 실행 중인 Codex CLI 세션을 다시 시작하거나 IDE 창을 다시 로드하고 새 세션을 여세요. 기존 프로세스는 이전 계정을 유지할 수 있습니다. Cursor 데스크톱이 실행 중이면 계정 전환 시 앱을 닫고 다시 엽니다.

<details>
<summary>추가 명령</summary>

```bash
subswap swap alice@example.com
subswap swap claude/alice@example.com

subswap add-api         # Claude Code 호환 API 엔드포인트 추가
subswap autoswap on     # 자동 전환 켜기
subswap autoswap off    # 수동 모드로 돌아가기
subswap doctor         # 로컬 경로와 클라이언트 설정 확인

subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

</details>

## 기능

- **저장된 계정 전환** — `subswap swap <번호>`로 선택하여 전환할 때마다 다시 로그인하는 수고를 줄입니다.
- **사용량과 초기화 시간 확인** — 지원 클라이언트의 남은 사용량과 함께 Codex reset 잔여 횟수 및 서비스가 제공하는 만료 시간을 표시합니다.
- **Claude Code 커스텀 API 사용** — `subswap add-api`로 Anthropic 호환 엔드포인트를 추가합니다. 이 계정은 수동으로 선택합니다.
- **다른 계정 병렬 사용** — 격리를 지원하는 클라이언트에서 `run`, `shell`, `env`를 사용하여 글로벌 활성 로그인을 유지합니다.
- **자동 전환 켜기** — 설정된 사용량 조건에 도달하면 다른 저장된 계정을 선택합니다.

## 지원 클라이언트

| 클라이언트 / 계정 유형 | 가져오기·전환 | 사용량 | 자동 전환 | 격리 실행 | 참고 |
|---|---:|---:|---:|---:|---|
| Claude Code(OAuth) | 지원 | 지원 | 지원 | 지원 | 커스텀 API 엔드포인트는 수동 선택만 지원. |
| Codex(ChatGPT 로그인) | 지원 | 지원 | 지원 | 지원 | 글로벌 전환 후 기존 Codex 세션을 다시 시작. |
| Kimi Code | 지원 | 지원 | 지원 | 지원 | 네이티브 클라이언트 로그인 후 가져오기. |
| Cursor 데스크톱 / CLI | 지원 | 지원 | 지원 | 미지원 | 데스크톱 전환은 앱 재시작을 조정. |
| OpenCode Console | 지원 | 지원 | 지원 | 미지원 | 공식 Console 계정 사이에서만 자동 전환. |
| OpenCode Go API key | 지원 | 지원 | 미지원 | V1만 | V2 키 선택은 공식 클라이언트 이용. |
| Command Code | 지원 | 지원 | 지원 | 지원 | 네이티브 클라이언트 로그인 후 가져오기. |

CLI는 macOS, Linux, Windows CI에서 테스트됩니다. 사용할 네이티브 클라이언트를 설치하세요. 같은 계정의 자격 증명을 여러 세션에서 동시에 갱신하면 다시 로그인해야 할 수 있습니다. 병렬 작업에는 가능한 한 서로 다른 저장된 계정을 사용하세요.

## 자동 전환

`subswap autoswap off`는 기본 명령과 백그라운드 daemon의 자동 전환을 모두 끕니다. `subswap autoswap on`으로 켭니다. 수동으로 선택한 후에는 설정 가능한 유지 기간 동안 자동 전환이 중지됩니다.

현재 계정의 사용량을 조회 중이거나, 조회에 실패하거나, 캐시가 오래된 경우 현재 계정을 유지합니다. 일반적으로 설정된 한도에 도달하면 사용 가능한 계정을 선택합니다. 현재 계정의 소진이 확인되고 사용 가능한 대상으로 확인된 계정이 없으면, 소진되었더라도 더 빨리 회복되는 것으로 확인된 계정을 선택할 수 있습니다. 한도와 시간 설정은 [설정 안내](docs/CONFIG.md)를 참고하세요.

백그라운드 daemon은 Unix 전용입니다. Linux는 `subswap` 실행 시 자동으로 시작하고 macOS는 `SUBSWAP_AUTO_DAEMON=1`이 필요합니다. Windows는 CLI를 직접 실행합니다. `SUBSWAP_NO_DAEMON=1`은 daemon의 자동 시작만 막습니다. 이미 실행 중인 daemon을 종료하거나 기본 명령 실행 시의 자동 전환을 끄지는 않습니다.

<details>
<summary>환경 확인 (`subswap doctor`)</summary>

<p align="center">
  <img src="docs/images/demo-doctor.gif" width="920" alt="터미널 데모: subswap doctor로 로컬 경로와 클라이언트 자격 증명 확인">
</p>

</details>

## 자격 증명

저장된 자격 증명은 계정 메타데이터와 별도로 subswap의 사용자별 앱 데이터 디렉터리에 평문 파일로 보관됩니다. macOS와 Linux의 비공개 자격 증명 및 스냅샷은 `0600` 권한을 사용합니다. Windows는 현재 사용자의 앱 데이터 권한을 사용합니다. 실제 경로는 `subswap doctor`에서 확인할 수 있습니다. 네이티브 클라이언트도 활성 자격 증명을 보관합니다. 커스텀 Claude API 모드는 Claude Code 설정에 API key를 기록하며, OAuth로 돌아가면 API 모드 이전의 관리 대상 설정을 복원합니다.

본인이 소유하거나 사용 권한이 있는 계정만 관리하세요.

## FAQ

### 수동 전환은 사용량 API에 의존하나요?

계정 변경 자체는 사용량 조회에 의존하지 않습니다. 전환에 성공한 뒤 사용량 표를 출력하며 캐시가 오래되었으면 네트워크로 조회할 수 있습니다. 사용량 조회가 실패해도 완료된 계정 변경은 취소되지 않습니다. 대상을 지정하지 않은 `subswap swap`은 저장된 계정만 나열하고 사용량을 조회하지 않습니다.

### ChatGPT 브라우저 로그인도 변경하나요?

Codex가 사용하는 ChatGPT 로그인을 관리합니다. ChatGPT 브라우저 세션은 별개입니다.

### Cursor에서 `run`, `shell`, `env`를 사용할 수 있나요?

Cursor는 데스크톱 또는 CLI 자격 증명으로 가져오기, 전환, 사용량 조회를 지원합니다. 이 격리 실행 명령은 지원하지 않습니다.

## 기여와 보안

기여 방법과 로컬 검사는 [CONTRIBUTING.md](CONTRIBUTING.md)를 참고하세요. 공개 Issue에 자격 증명, refresh token, 로그인 파일, 실제 이메일 주소 또는 결제 화면을 올리지 마세요. 비공개 취약점 보고는 [SECURITY.md](SECURITY.md)를 참고하세요.

도움이 되었다면 [star](https://github.com/x0c/subswap)를 남겨 두면 나중에 다시 찾기 쉽습니다.

## 라이선스

MIT — [LICENSE](LICENSE)를 참고하세요.
