# Conga Desktop

A Tauri + Vue 3 + TypeScript desktop application.

## Prerequisites

- [Node.js](https://nodejs.org/)
- [pnpm](https://pnpm.io/)
- [Rust](https://www.rust-lang.org/)

## Scripts

```bash
# Start the desktop app in development mode
pnpm tauri:dev

# Build the desktop app for distribution
pnpm tauri:build

# Run only the Vite dev server (browser mode)
pnpm dev

# Build only the frontend assets
pnpm build
```

## Project Structure

- `src/` – Vue frontend source code
- `src-tauri/` – Tauri Rust backend and app configuration
- `dist/` – Built frontend assets consumed by Tauri
