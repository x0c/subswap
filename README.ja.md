# subswap - Claude、Codex、ChatGPT アカウント切り替えツール

Languages: [English](README.md) | [简体中文](README.zh-CN.md) | 日本語 | [한국어](README.ko.md)

subswap は、Claude Code、Anthropic Claude、OpenAI Codex CLI、ChatGPT の複数の AI サブスクリプションアカウントを管理するための Rust CLI です。ローカルのログイン状態を取り込み、認証情報を OS の keyring に保存し、クォータウィンドウを確認し、使用量が設定済みしきい値を超えたときに手動または自動でアクティブアカウントを切り替えます。

Claude アカウント切り替えツール、Codex アカウント管理ツール、ChatGPT クォータトラッカー、または複数 Provider を統合するサブスクリプション切り替えツールとして利用できます。

## 機能

- **Claude Code と Codex CLI のマルチアカウント切り替え**：再ログインなしでアクティブアカウントを切り替えます。
- **クォータ対応ステータス**：利用可能な場合、Claude の 5h / 7d 使用量や Codex / ChatGPT の使用データなど、Provider のクォータウィンドウを表示します。
- **自動アカウント切り替え**：バックグラウンド daemon が、使用量が設定済みしきい値を超えたアカウントから切り替えます。
- **ネットワークに依存しない手動切り替え**：クォータ API の失敗、token の期限切れ、ネットワーク切断があっても `subswap swap` は動作します。
- **keyring ベースの認証情報保存**：シークレットは macOS Keychain、Windows Credential Manager、Linux secret-service に保存されます。
- **Provider ベースのアーキテクチャ**：Claude / Anthropic と Codex / ChatGPT は別々の crate なので、core のポリシーを変えずに新しい AI Provider を追加できます。

## 対応クライアント

| Provider | ローカルクライアント | subswap が管理するもの |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth 認証情報、アクティブアカウントファイル、5h / 7d クォータ、token keepalive |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json` パススルー、アクティブアカウントファイル、ChatGPT 使用量検索 |

## よくある用途

- 複数の Claude Pro、Claude Max、ChatGPT Plus、ChatGPT Team シートを切り替える。
- 現在のアカウントが利用上限に達したときのために、予備の AI サブスクリプションを待機させておく。
- 長いコーディングセッションを始める前に、各アカウントの使用量を確認する。
- Claude と ChatGPT のアカウント切り替えを 1 つの CLI にまとめる。

## ステータス

| マイルストーン | 範囲 | 状態 |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: keyring-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json passthrough, atomic write, tolerant wham/usage parsing | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |

## なぜ必要か

複数の AI サブスクリプションを利用していると、次のような状況が起こりがちです。

- Claude Pro の利用枠を使い切り、再ログインせずに ChatGPT へ切り替えたい。
- 2 つの ChatGPT シートを持っていて、1 行のコマンドでアクティブなものを切り替えたい。
- 各アカウントのウィンドウ（5h / 7d）ごとの残りを確認したい。

subswap は各アカウントを OS の keyring（Keychain / Credential Manager / secret-service）に保存し、同じオンディスク認証情報ファイルを読むすべてのクライアントでアクティブアカウントをアトミックに切り替えます。手動切り替えはネットワークでブロックされず、クォータ検索は参考情報として扱われます。

## インストール

Homebrew を使う場合:

```bash
brew install x0c/tap/subswap
```

先に tap してから名前でインストールすることもできます。

```bash
brew tap x0c/tap
brew install subswap
```

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

# remove an account from the registry and the keyring
subswap rm alice@example.com

# environment self-check (client files, keyring, config dirs)
subswap doctor
```

Claude Code / Codex CLI に少なくとも一度ログインしていれば、初回実行時に `~/.claude` と `~/.codex` からアカウントが自動的に取り込まれます。

最初の `subswap` 実行時には、macOS 以外の Unix プラットフォームで分離されたバックグラウンド daemon（`subswapd`）も起動します。daemon はクォータをポーリングしてバックグラウンドで自動切り替えを行い、Claude OAuth token を新鮮に保つことで、長く使っていなかったアカウントへ切り替えた瞬間に 401 になることを避けます。macOS では、切り離されたプロセスの Keychain アクセスが追加の認証プロンプトを出すことがあるため明示的な opt-in が必要です。自動起動を有効にするには `SUBSWAP_AUTO_DAEMON=1` を export してください。daemon は単一インスタンス（ファイルロック）です。停止しても問題ありません：`pkill -f 'subswap __daemon'` または `pkill subswapd`。完全に無効化するには `SUBSWAP_NO_DAEMON=1` を export してください。

## 設計上の不変条件

貢献前に把握しておきたい重要な前提です。

1. **`swap` はクォータ検索に依存しません。** API が停止している、keyring にアクセスできない、token が期限切れである場合でも、手動切り替えはアクティブアカウントの切り替えを試みる必要があります。
2. **シークレットは OS keyring のみに保存されます。** `registry.toml`、監査ログ、スナップショットには平文の token や refresh token を含めません。
3. **切り替えはアトミックで、ロールバック可能です。** 各 `activate` は変更前に `state_dir/snapshots/<ts>/` にスナップショットを書き込みます。いずれかの書き込みが失敗した場合はロールバックします。
4. **Provider を追加するには `crates/providers/<id>` crate を追加し、`cli/src/main.rs::AppContext::build()` に 1 行登録します。** `core` に Provider 固有ロジックは置きません。
5. **自動切り替えしきい値は集中管理され、設定可能です。** コンパイル時のデフォルトは `crates/core/src/defaults.rs` にあり、実行時設定で上書きできます。

詳細：[`docs/`](docs/)（中国語の内部コラボレーション文書）。

## 比較

| ツール | 主な用途 | subswap との違い |
|---|---|---|
| 単一 Provider のアカウント切り替えツール | 1 つの上流のみを対象 | subswap は 1 つの Provider 抽象で Claude と Codex / ChatGPT をサポート |
| クォータダッシュボード | 使用量の可視化のみ | subswap はクォータウィンドウが埋まったときに別のローカルアカウントをアクティブ化可能 |
| 手動ログイン/ログアウト | 一度に 1 アカウント | subswap は登録済みアカウントを keyring に保持し、ローカルファイルをアトミックに切り替え |

## FAQ

### `subswap swap` はクォータ API を呼びますか？

いいえ。手動切り替えは避難経路であり、クォータ検索に依存しません。上流 API が停止している場合や token が期限切れの場合でも、`subswap swap claude/alice@example.com` はそのローカルアカウントのアクティブ化を試みます。

### token はどこに保存されますか？

token と refresh token は OS keyring のみに保存されます。`registry.toml`、監査ログ、スナップショットには平文シークレットを含めない設計です。

### Claude 専用ですか？

いいえ。最初に対応する Provider は Claude / Anthropic と Codex / ChatGPT です。core crate は Provider trait を公開しているため、将来の AI サブスクリプション Provider は別 crate として追加できます。

## GitHub topics

公開後に推奨するリポジトリ topics：

`claude-code`, `codex-cli`, `chatgpt`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `keyring`, `automation`

## レイアウト

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
```

## コントリビューション

Issues と PR を歓迎します。注意事項：

- `docs/` と `AGENTS.md` の内部文書は中国語です。コードコメントは中国語です。ユーザーに見える内容（CLI テキスト、エラーメッセージ、tracing ログ、crate description）は英語です。
- PR を開く前に `cargo check --workspace` と `cargo test --workspace` を実行してください。

## License

MIT — [`LICENSE`](LICENSE) を参照してください。
