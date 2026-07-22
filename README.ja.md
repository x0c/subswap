# subswap - Claude、Codex、ChatGPT、Kimi、Cursor アカウント切り替えツール

Languages: [English](README.md) | [简体中文](README.zh-CN.md) | 日本語 | [한국어](README.ko.md)

subswap は、Claude Code、OpenAI Codex / ChatGPT、Kimi Code、Cursor の複数の AI サブスクリプションアカウントを管理する Rust CLI です。ローカルのログイン状態を取り込み、クォータを確認し、アクティブアカウントを手動または自動で切り替えます。

Claude アカウント切り替えツール、Codex アカウント管理ツール、ChatGPT クォータトラッカー、または複数 Provider を統合するサブスクリプション切り替えツールとして利用できます。

**プラットフォームサポート**: CLI と 4 Provider は macOS / Linux / Windows の CI で検証されています。バックグラウンド daemon は Unix 専用で、Windows ではフォアグラウンド CLI を使用します。

## 機能

- **Claude Code、Codex CLI、Kimi Code、Cursor のマルチアカウント切り替え**：再ログインなしでアクティブアカウントを切り替えます。
- **Claude Code カスタム API エンドポイント**：インタラクティブウィザードで DeepSeek、Kimi などの Anthropic 互換エンドポイントを追加し、通常の Claude アカウントと同様に切り替えられます。
- **Claude / Codex / Kimi のアカウント分離環境**：`subswap run`・`shell`・`env` で並列利用できます。Cursor はデスクトップ SQLite のためこのモードには対応しません。
- **クォータ対応ステータス**：Claude / Kimi / Codex のウィンドウと、Cursor の `First-Party Models` / `API` 使用率を表示します。
- **自動アカウント切り替え**：バックグラウンド daemon が使用量がしきい値を超えたアカウントから切り替え、クォータ更新のたびに再判定して常に最良のアカウントを選びます。
- **自動切り替えトグル**：`subswap autoswap on/off` でコンフィグファイルを触らずに自動切り替えを有効・無効化できます。
- **手動切り替え後のセトルグレース**：手動でアカウントを選んだ後、daemon はグレース期間を置いてから自動切り替えを行い、意図がすぐに上書きされることを防ぎます。
- **ネットワークに依存しない手動切り替え**：クォータ API の失敗、token の期限切れ、ネットワーク切断があっても `subswap swap` は動作します。
- **クォータ結果キャッシュと stale fallback**：バックグラウンド取得中もキャッシュ結果を返すため、ステータス画面は常に応答します。
- **ファイルベースの認証情報保存**：トークンはアプリデータディレクトリ内のオーナーのみ読み取り可能な `0600` ファイルに保存されます。古い keyring ベースのインストールからの移行は初回実行時に自動で行われます。
- **Provider ベースのアーキテクチャ**：Claude、Codex、Kimi、Cursor は別々の crate です。

## 対応クライアント

| Provider | ローカルクライアント | subswap が管理するもの |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth 認証情報、カスタム API エンドポイント、アクティブアカウントファイル、5h / 7d クォータ、token keepalive |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json`、アクティブアカウント、公式 app-server クォータ |
| Kimi / Moonshot | Kimi Code (`~/.kimi-code`) | OAuth 認証情報、アクティブアカウント、5h / 7d 使用量 |
| Cursor | Cursor デスクトップ (`state.vscdb`) | アカウント切り替え、First-Party Models / API 使用率、請求サイクルリセット |

## よくある用途

- 複数の Claude Pro、Claude Max、ChatGPT Plus、ChatGPT Team シートを切り替える。
- 現在のアカウントが利用上限に達したときのために、予備の AI サブスクリプションを待機させておく。
- 異なるターミナルで 2 つのアカウントを同時に使う。
- 長いコーディングセッションを始める前に、各アカウントの使用量を確認する。
- Claude、ChatGPT、Kimi、Cursor のアカウント切り替えを 1 つの CLI にまとめる。

## ステータス

| マイルストーン | 範囲 | 状態 |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: credential-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json, atomic write, official quota + fallback | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |
| M5 | アカウント分離実行環境、自動切り替えトグル、クォータキャッシュ、セトルグレース | done |
| M6 | Kimi / Cursor Provider、Codex 公式クォータ経路、安全な token 復旧 | done |

## なぜ必要か

複数の AI サブスクリプションを利用していると、次のような状況が起こりがちです。

- Claude Pro の利用枠を使い切り、再ログインせずに ChatGPT へ切り替えたい。
- 2 つの ChatGPT シートを持っていて、1 行のコマンドでアクティブなものを切り替えたい。
- 2 つのアカウントを異なるターミナルで干渉なく同時に使いたい。
- 各アカウントのウィンドウ（5h / 7d）ごとの残りを確認したい。

subswap は各アカウントの認証情報をオーナー専用ファイルに保存し、各ネイティブクライアントのアクティブ状態をトランザクションで更新します。手動切り替えはクォータ検索にブロックされません。

## インストール

### macOS / Linux (Homebrew)

Homebrew を使う場合:

```bash
brew install x0c/tap/subswap
```

先に tap してから名前でインストールすることもできます。

```bash
brew tap x0c/tap
brew install subswap
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

`subswap.exe` をインストールしてユーザー `PATH` に追加します。Windows 版には Unix 専用 daemon は含まれません。

### ソースから

ソースからインストールする場合は Rust 1.80+ が必要です。

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

Git から直接インストールすることもできます。

```bash
cargo install --git https://github.com/x0c/subswap --path crates/cli
```

## クイックスタート

```bash
# default: sync local active accounts, fetch quotas, auto-swap if past threshold,
# then print a one-screen status. Run this whenever you want to know what's up.
subswap

# manually swap to a specific account (escape hatch — never depends on the network)
subswap swap alice@example.com
# disambiguate when the same id exists under multiple providers:
subswap swap claude/alice@example.com

# インタラクティブに DeepSeek などの Claude Code 互換 API を追加
subswap add-api
# カスタム API エンドポイントは手動専用 — 自動切り替えには参加しない
subswap swap deepseek

# グローバルのアクティブアカウントを変えずに分離環境でアカウントを使う
subswap run codex bob@example.com -- --version   # bob のアカウントで codex を分離起動
subswap shell alice@example.com                  # alice のアカウントに分離したサブシェルを開く
eval "$(subswap env codex/bob@example.com)"      # 現在のシェルを一時的に codex アカウントに向ける

# 自動切り替えを有効化 / 無効化
subswap autoswap on
subswap autoswap off

# アカウントを registry と非公開認証情報ストアから削除
subswap rm alice@example.com

# 環境セルフチェック (クライアントファイル、keyring、設定ディレクトリ)
subswap doctor
```

各ネイティブクライアントで一度ログインしていれば、初回実行時に Claude Code、Codex CLI、Kimi Code、Cursor の現在のアカウントを自動的に取り込みます。

最初の `subswap` 実行時には、macOS 以外の Unix プラットフォームで分離されたバックグラウンド daemon（`subswapd`）も起動します。daemon はクォータをポーリングしてバックグラウンドで自動切り替えを行い、Claude OAuth token を新鮮に保つことで、長く使っていなかったアカウントへ切り替えた瞬間に 401 になることを避けます。macOS では、切り離されたプロセスの Keychain アクセスが追加の認証プロンプトを出すことがあるため明示的な opt-in が必要です。自動起動を有効にするには `SUBSWAP_AUTO_DAEMON=1` を export してください。daemon は単一インスタンス（ファイルロック）です。停止しても問題ありません：`pkill -f 'subswap __daemon'` または `pkill subswapd`。完全に無効化するには `SUBSWAP_NO_DAEMON=1` を export してください。

## アカウント分離環境

`subswap run / shell / env` は Claude、Codex、Kimi をグローバルのアクティブアカウントを変えずに並列使用します。Cursor は SQLite とアプリ再起動の協調が必要なため対象外です。

```bash
subswap run codex 6 -- --version       # アカウント #6 で分離して codex を実行
subswap run claude alice@x.com         # alice のアカウントで claude を分離起動
subswap shell 3                         # アカウント #3 に分離したサブシェルに入る
eval "$(subswap env codex/bob@x.com)"  # 現在のシェルを一時的に codex アカウントに向ける
```

- **並行利用のトレードオフ**：同じアカウントを複数の分離セッションで利用できますが、同時 refresh が発生すると一方で再ログインが必要になる場合があります。
- **グローバルアクティブ警告**：現在のグローバルアクティブアカウントに対して分離セッションを開始すると警告が表示されます — 非分離クライアントが同時に使用している場合、refresh token が無効化される可能性があります。

## 設計上の不変条件

貢献前に把握しておきたい重要な前提です。

1. **`swap` はクォータ検索に依存しません。** API が停止している、keyring にアクセスできない、token が期限切れである場合でも、手動切り替えはアクティブアカウントの切り替えを試みる必要があります。
2. **シークレットはレジストリメタデータに入らず、スナップショットはオーナーのみ読み取り可能です。** OAuth/API シークレットはオーナーのみの認証情報ストアに保存されます。カスタム API がアクティブな間は、Claude Code も `~/.claude/settings.json` に API キーが必要です。subswap はそのファイルをアトミックに保存し、OAuth に戻る際に復元します。
3. **切り替えはアトミックで、ロールバック可能です。** 各 `activate` は変更前に `state_dir/snapshots/<ts>/` にスナップショットを書き込みます。いずれかの書き込みが失敗した場合はロールバックします。
4. **Provider を追加するには `crates/providers/<id>` crate を追加し、`cli/src/app.rs::AppContext::build()` に登録します。** `core` に Provider 固有ロジックは置きません。
5. **自動切り替えしきい値は集中管理され、設定可能です。** コンパイル時のデフォルトは `crates/core/src/defaults.rs` にあり、実行時設定で上書きできます。

詳細：[`docs/`](docs/)（中国語の内部コラボレーション文書）。

## 比較

| ツール | 主な用途 | subswap との違い |
|---|---|---|
| 単一 Provider のアカウント切り替えツール | 1 つの上流のみを対象 | subswap は Claude、Codex / ChatGPT、Kimi、Cursor をサポート |
| クォータダッシュボード | 使用量の可視化のみ | subswap はクォータウィンドウが埋まったときに別のローカルアカウントをアクティブ化可能 |
| 手動ログイン/ログアウト | 一度に 1 アカウント | subswap は登録済みアカウントを保持し、ローカルファイルをアトミックに切り替え |

## FAQ

### `subswap swap` はクォータ API を呼びますか？

いいえ。手動切り替えは避難経路であり、クォータ検索に依存しません。上流 API が停止している場合や token が期限切れの場合でも、`subswap swap claude/alice@example.com` はそのローカルアカウントのアクティブ化を試みます。

### token はどこに保存されますか？

token と refresh token はアプリデータディレクトリ内のオーナーのみ読み取り可能な認証情報ファイルに保存されます。カスタム API がアクティブな間は、Claude Code も `~/.claude/settings.json` に API キーが必要です。切り替えスナップショットも同様に `0600` に制限されます。

### カスタム API は自動切り替えに参加しますか？

いいえ。カスタム API は `manual_only` です。subswap が自動的に選択することはなく、アクティブな間は自動切り替えも完全に無効化されます。OAuth アカウントに手動で戻すと、API モードに入る前の Claude Code 設定が復元されます。

### Claude / Codex 専用ですか？

いいえ。Claude / Anthropic、Codex / ChatGPT、Kimi / Moonshot、Cursor に対応しています。

### Windows で動きますか？

はい。CLI と 4 Provider は Windows CI で検証され、上記 PowerShell コマンドでインストールできます。daemon のみ Unix 専用です。

## GitHub topics

公開後に推奨するリポジトリ topics：

`claude-code`, `codex-cli`, `chatgpt`, `kimi`, `moonshot-ai`, `cursor`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `automation`

## レイアウト

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
    kimi/              # Kimi / Moonshot provider
    cursor/            # Cursor provider
```

## コントリビューション

Issues と PR を歓迎します。注意事項：

- `docs/` と `AGENTS.md` の内部文書は中国語です。コードコメントは中国語です。ユーザーに見える内容（CLI テキスト、エラーメッセージ、tracing ログ、crate description）は英語です。
- PR を開く前に `cargo check --workspace` と `cargo test --workspace` を実行してください。

## License

MIT — [`LICENSE`](LICENSE) を参照してください。
