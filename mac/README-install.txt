Fluent Gallery (macOS / Apple Silicon)

1. Fluent Gallery.app を Applications へドラッグ
2. 初回起動でモデルを取得します(内蔵LLM 2.5GB、内蔵VLM 3.3GB、生成モデル約7GB など。設定 → AI から個別に)
3. データは ~/Library/Application Support/FluentGallery/ に置かれます

署名・公証済みのビルドはそのまま開けます。テストビルド(未署名)の場合だけ、
  xattr -dr com.apple.quarantine "/Applications/Fluent Gallery.app"
か、システム設定 → プライバシーとセキュリティ → 「このまま開く」で開いてください。
