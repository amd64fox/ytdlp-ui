# ytdlp-ui

Windows GUI для загрузки через yt-dlp с управлением версиями yt-dlp, ffmpeg, ffprobe и самого приложения.

## Выпуск Windows x64

Workflow `.github/workflows/release.yml` запускается для тегов `v*` или вручную. Версия тега должна совпадать с версией пакета в `Cargo.toml`.

Каждый выпуск содержит:

- `ytdlp-ui-x64.exe` — Windows x64 сборка

GitHub автоматически рассчитывает SHA-256 для release asset. Встроенный updater получает digest через GitHub Releases API, скачивает новую версию во временный файл, проверяет её, закрывает приложение, заменяет EXE и запускает обновлённую версию.
