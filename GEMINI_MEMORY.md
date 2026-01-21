# BaiTTS-CLI-rs Project Memory

This document stores key architectural and feature information about the `baitts-cli-rs` project for future reference.

## Core Architecture
- The application is a Rust-based CLI with an optional web UI mode.
- It relies on an external `MultiTTS` API for all text-to-speech functionality.
- The project uses `clap` for CLI argument parsing.
- The web server is built with `axum` and runs on port `5688` by default.
- Asynchronous operations are handled by `tokio`.
- It can process `.txt` and `.epub` files. `epub` parsing is done with the `epub` crate.
- Audio is written as `.mp3` files. MP3 encoding is handled by calling an external `ffmpeg` process. ID3 tags are written using the `id3` crate.
- **Dialogue Separation**: Supports regex-based separation of dialogue and narrator text. Allows distinct voice IDs (`--voice-dialogue`) and audio parameters (volume, speed, pitch) for dialogue segments.
- **Text Filtering**: Supports custom regex-based content filtering (`--ignore-regex`) to remove unwanted text patterns before synthesis.

## Web UI Features
- The Web UI is launched with the `--web` flag.
- **State Management**: The server maintains a shared state (`AppState`) containing:
    - An SSE broadcaster (`tx`) for real-time logging to the UI.
    - A handle for abortable tasks (`task_handle`).
    - An `InitialData` struct that holds the last known valid `API_URL` and the corresponding voice list.
- **API URL Handling**:
    - The `API_URL` can be pre-configured by setting an environment variable of the same name.
    - If set, the server fetches the voice list on startup.
    - If a user manually enters a URL in the UI and successfully fetches the voice list, the server updates its in-memory `InitialData` state to "remember" this valid URL for the current session.
- **Batch Conversion**:
    - A batch conversion feature is available in the UI.
    - It is designed to work with Docker volumes. The backend reads from `/book` and writes to `/output`.
    - Supports configuring dialogue-specific voice and audio parameters.
    - The feature is triggered by a `POST` request to the `/api/batch_convert` endpoint.
    - It uses the `walkdir` crate to find compatible book files.
    - The conversion runs in background `tokio` tasks.
- **Task Management**:
    - Real-time progress tracking for file conversions (chapters processed/total).
    - Supports Cancel, Retry, Retry All Failed, Clear Completed, and Clear All operations.
    - Task state is persisted to `baitts_tasks.json` in the output directory to survive container restarts.
    - Detailed task view with configuration inspection and "Copy Config" functionality.
    - Filtering by status and searching by filename.
- **Interactive Features**:
    - **Preview**: `/api/preview` endpoint allows real-time audio preview for text input without saving files. Includes download capability.
    - **Regex Tester**: A tool to test and verify the `ignore_regex` pattern against sample text.
    - **UX**: Dark mode support, "Reset to Defaults" button, and LocalStorage persistence for settings.

## Docker Integration
- A `Dockerfile` is provided for containerized deployment.
- The image is built with `docker build -t baitts-cli-rs .`.
- **Multi-Arch Support**: Can be built for ARM64 using `docker buildx build --platform linux/arm64 ...`.
- It uses a `docker-entrypoint.sh` script, but most startup logic (like API checking) has been moved into the Rust server itself.
- Declared volumes: `/book` (for input) and `/output` (for output).
- The default `CMD` is to start the web UI (`--web`).
- **Environment Variables**: Supports `DEFAULT_VOLUME`, `DEFAULT_SPEED`, and `DEFAULT_PITCH` to set default audio parameters on startup.