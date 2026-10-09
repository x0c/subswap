<p align="center">
  <a href="README.md"><img src="https://img.shields.io/badge/English-gray" alt="English"></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-gray" alt="简体中文"></a>
  <a href="README.ja.md"><img src="https://img.shields.io/badge/%E6%97%A5%E6%9C%AC%E8%AA%9E-%E2%9C%93-blue" alt="日本語"></a>
  <a href="README.ko.md"><img src="https://img.shields.io/badge/%ED%95%9C%EA%B5%AD%EC%96%B4-gray" alt="한국어"></a>
</p>

<h1 align="center">subswap</h1>

<p align="center"><strong>AI コーディングツールの残り利用枠を確認し、アカウントを切り替え。</strong></p>

<p align="center">Claude Code、Codex、Kimi Code、Cursor、OpenCode、Command Code のログインを保存して管理します。利用枠が残っているアカウントを確認し、コマンド一つで切り替えられます。</p>

<p align="center">
  <a href="https://github.com/x0c/subswap/actions/workflows/ci.yml"><img src="https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/x0c/subswap/actions/workflows/release.yml"><img src="https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/x0c/subswap" alt="License"></a>
</p>

<p align="center">
  <img src="docs/images/demo-swap.gif" width="920" alt="ターミナルのデモ：一覧を表示し、subswap swap で別の Codex アカウントに切り替え">
</p>
<p align="center"><em>サンプルアカウントを使ったデモです。</em></p>

## インストール

**Homebrew がインストールされた macOS または Linux**：

Linux のビルド済みパッケージには glibc 2.39 以降が必要です。古い環境ではソースからビルドしてください。

```bash
brew install x0c/tap/subswap
subswap --help
```

<details>
<summary>Windows / ビルド済みパッケージ / ソースから</summary>

**Windows**

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
subswap --help
```

インストーラーは最新の Windows Release をダウンロードし、SHA-256 を検証して `subswap.exe` をユーザーの `PATH` に追加します。[最新 Release](https://github.com/x0c/subswap/releases/latest) から zip とチェックサムを取得することもできます。

**ビルド済みパッケージ**

[最新 Release](https://github.com/x0c/subswap/releases/latest) は macOS（Apple silicon / Intel）、Linux（ARM64 / x64）、Windows（x64）向けです。インストール前に同梱の SHA-256 を検証してください。

**ソースから**（現在の安定版 Rust、開発向け）

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --locked --path crates/cli
subswap --help
```

通常利用には Homebrew または Release のパッケージを推奨します。

</details>

## 初回利用

まず対応するネイティブクライアントをインストールしてログインしてください。自動切り替えは初期設定で有効です。自分でアカウントを選ぶには、手動モードから始めます。

```bash
subswap autoswap off
subswap                # ローカルのログインを保存し、アカウントと利用枠を表示
```

アカウントを切り替える前に、もう一つのアカウントを保存します。Claude Code と Codex では、subswap からネイティブのログインを開始できます。

```bash
subswap login codex    # 別の Codex アカウントでログイン。Claude Code は claude を指定
subswap swap 2         # 2 をアカウント一覧の番号に置き換える
```

Kimi Code、Cursor、Command Code は、ネイティブクライアントで別のアカウントにログインした後、`subswap login kimi`、`subswap login cursor`、`subswap login commandcode` で保存します。`subswap login opencode` は公式 Console アカウントを取り込み、必要に応じて公式のログインを開始します。`subswap login opencode-api-key` は Go API key を別の一覧に取り込みます。

Codex を切り替えた後は、実行中の Codex CLI セッションを再起動するか、IDE ウィンドウを再読み込みして新しいセッションを開いてください。既存のプロセスは以前のアカウントを保持する場合があります。Cursor デスクトップが実行中の場合は、切り替え時にアプリを終了して再起動します。

<details>
<summary>その他のコマンド</summary>

```bash
subswap swap alice@example.com
subswap swap claude/alice@example.com

subswap add-api         # Claude Code 互換 API エンドポイントを追加
subswap autoswap on     # 自動切り替えを有効化
subswap autoswap off    # 手動モードに戻す
subswap doctor         # ローカルパスとクライアント設定を確認

subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

</details>

## 機能

- **保存したアカウントを切り替え** — `subswap swap <番号>` で選択し、切り替えのたびにログインし直す手間を減らします。
- **利用枠とリセット時刻を確認** — 対応クライアントの残り利用量に加え、Codex reset の利用可能回数とサービスから提供された有効期限を表示します。
- **Claude Code のカスタム API を利用** — `subswap add-api` で Anthropic 互換エンドポイントを追加します。これらのアカウントは手動で選択します。
- **別のアカウントを並列利用** — 分離実行に対応するクライアントで `run`、`shell`、`env` を使い、グローバルのログインを変更せずに利用できます。
- **自動切り替えを有効化** — 設定した利用枠の条件を満たすと、別の保存済みアカウントを選択します。

## 対応クライアント

| クライアント / アカウント種別 | 取り込み・切り替え | 利用枠 | 自動切り替え | 分離実行 | 備考 |
|---|---:|---:|---:|---:|---|
| Claude Code（OAuth） | 対応 | 対応 | 対応 | 対応 | カスタム API エンドポイントは手動選択のみ。 |
| Codex（ChatGPT ログイン） | 対応 | 対応 | 対応 | 対応 | グローバル切り替え後は既存の Codex セッションを再起動。 |
| Kimi Code | 対応 | 対応 | 対応 | 対応 | ネイティブクライアントでログイン後に取り込み。 |
| Cursor デスクトップ / CLI | 対応 | 対応 | 対応 | 非対応 | デスクトップの切り替えはアプリ再起動を調整。 |
| OpenCode Console | 対応 | 対応 | 対応 | 非対応 | 自動切り替えは公式 Console アカウント間のみ。 |
| OpenCode Go API key | 対応 | 対応 | 非対応 | V1 のみ | V2 のキー選択は公式クライアント経由。 |
| Command Code | 対応 | 対応 | 対応 | 対応 | ネイティブクライアントでログイン後に取り込み。 |

CLI は macOS、Linux、Windows の CI でテストされています。利用するネイティブクライアントをインストールしてください。同じアカウントを複数のセッションで使い、認証情報の更新が重なると、再ログインが必要になる場合があります。並列利用では可能な限り別々の保存済みアカウントを選んでください。

## 自動切り替え

`subswap autoswap off` はデフォルトコマンドとバックグラウンド daemon の自動切り替えを無効化します。`subswap autoswap on` で有効化します。手動選択の後には設定可能な保持期間があり、その間は自動切り替えを停止します。

現在のアカウントの利用枠が取得中、取得失敗、または古いキャッシュの場合は、そのアカウントを維持します。通常は設定したしきい値に達すると利用可能なアカウントを選択します。現在のアカウントが確認済みの枯渇状態で、利用可能と確認できた候補がない場合は、枯渇していても回復が早いと確認されたアカウントを選ぶことがあります。しきい値と時間の設定は [設定ガイド](docs/CONFIG.md) を参照してください。

バックグラウンド daemon は Unix 専用です。Linux は `subswap` 実行時に自動起動し、macOS は `SUBSWAP_AUTO_DAEMON=1` が必要です。Windows は CLI を直接実行します。`SUBSWAP_NO_DAEMON=1` は daemon の自動起動だけを防ぎます。既に実行中の daemon を停止したり、通常のコマンド実行による自動切り替えを無効化したりはしません。

<details>
<summary>環境チェック（`subswap doctor`）</summary>

<p align="center">
  <img src="docs/images/demo-doctor.gif" width="920" alt="ターミナルのデモ：subswap doctor でローカルパスとクライアント認証情報を確認">
</p>

</details>

## 認証情報

保存した認証情報は、アカウントのメタデータとは別に、subswap のユーザー別アプリデータディレクトリへ平文ファイルとして保存されます。macOS と Linux の非公開認証情報とスナップショットは `0600` 権限を使用します。Windows は現在のユーザーのアプリデータ権限を使用します。実際のパスは `subswap doctor` で確認できます。ネイティブクライアントも現在の認証情報を保持します。カスタム Claude API モードでは Claude Code の設定に API key を書き込み、OAuth に戻すと API モード以前の管理対象設定を復元します。

自分が所有するか、利用権限のあるアカウントのみ管理してください。

## FAQ

### 手動切り替えは利用枠 API に依存しますか？

アカウント変更自体は利用枠の取得に依存しません。成功後は利用枠の一覧を表示し、キャッシュが古ければネットワークに問い合わせる場合があります。利用枠の取得に失敗しても、完了したアカウント変更は取り消しません。対象を指定しない `subswap swap` は保存済みアカウントの一覧だけを表示し、利用枠を取得しません。

### ChatGPT ブラウザーのログインも変更しますか？

管理するのは Codex が使用する ChatGPT ログインです。ChatGPT のブラウザーセッションは別です。

### Cursor で `run`、`shell`、`env` を使えますか？

Cursor はデスクトップまたは CLI の認証情報を使った取り込み、切り替え、利用枠の確認に対応します。これらの分離実行コマンドには対応しません。

## 貢献とセキュリティ

貢献の進め方とローカルチェックは [CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。公開 Issue に認証情報、refresh token、ログインファイル、実在のメールアドレス、請求画面を貼らないでください。脆弱性の非公開報告は [SECURITY.md](SECURITY.md) を参照してください。

役に立ったら [Star](https://github.com/x0c/subswap) しておくと、あとから見つけやすくなります。

## ライセンス

MIT — [LICENSE](LICENSE) を参照してください。
