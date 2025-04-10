# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

**Standard Development:**
- `cargo build` - Debug build
- `cargo build --release` - Release build
- `cargo test --all` - Run all tests
- `env RUN_SLOW_TESTS=1 cargo test --all` - Include comprehensive layout randomized tests
- `cargo clippy --all` - Lint all workspace crates

**Profiling and Performance:**
- `cargo build --release --features=profile-with-tracy-ondemand` - Tracy profiler (on-demand)
- `cargo build --release --features=profile-with-tracy` - Tracy profiler (always-on)
- Memory profiling: `--features=profile-with-tracy-allocations`

**Testing Commands:**
- Property-based testing: `env RUN_SLOW_TESTS=1 PROPTEST_CASES=200000 PROPTEST_MAX_GLOBAL_REJECTS=200000 RUST_BACKTRACE=1 cargo test --release --all`
- Visual testing: Run `cargo run` in `niri-visual-tests/` subdirectory

## Architecture Overview

**Workspace Structure:**
- `niri` - Main compositor executable
- `niri-config` - Configuration parsing using KDL format  
- `niri-ipc` - Inter-process communication types and client/server
- `niri-visual-tests` - GTK-based visual testing framework

**Core Components:**

**Layout System (`src/layout/`):**
- Scrollable tiling: horizontal strips with columns, no window resizing on new windows
- Dynamic workspaces: vertical arrangement, auto-create empty workspace at bottom
- Per-monitor independence: each monitor has separate window strips
- Key files: `workspace.rs`, `tile.rs`, `floating.rs`, `monitor.rs`

**Input System (`src/input/`):**
- Multi-device support: keyboard, mouse, touchpad, touch, tablet
- Gesture recognition for touchpad and mouse
- Various grab modes: move, resize, spatial movement, touch interactions
- **Input Inhibition Feature**: `input_inhibited` flag in main state allows completely blocking input processing for remote desktop/VM scenarios

**Rendering (`src/render_helpers/`):**
- GPU-accelerated with OpenGL ES
- Custom shaders in `shaders/` directory
- Damage tracking, multi-GPU support with primary GPU texture handling
- Visual effects: gradients (Oklab/Oklch), borders, shadows, animations

**Backend Support (`src/backend/`):**
- TTY backend for direct hardware (production)
- Winit backend for nested window (development)
- Headless backend for testing

**Protocol Implementation:**
- `src/handlers/` - Core Wayland protocols (compositor, XDG shell, layer shell)
- `src/protocols/` - Extensions (screencopy, gamma control, output management, etc.)

## Testing Infrastructure

**Test Types:**
- Unit tests for layout operations and config parsing
- Property-based testing with `proptest` for randomized layout operations
- Integration tests with mock Wayland clients
- Snapshot testing with `insta` for layout state verification
- Visual tests via GTK application in `niri-visual-tests/`

**Key Testing Patterns:**
- Layout tests use extensive randomized property testing to verify invariants
- Slow tests (`RUN_SLOW_TESTS=1`) include comprehensive randomized scenarios
- Visual tests allow manual verification of rendering and animations

## Development Guidelines

**Dependencies:**
- Built on Smithay Wayland compositor framework (git dependency)
- Uses `tracing` for structured logging throughout codebase
- Feature flags for optional functionality: `dbus`, `systemd`, `xdp-gnome-screencast`

**Code Patterns:**
- Live config reloading via file watching
- Comprehensive error handling designed to avoid compositor crashes
- Extensive use of feature flags for conditional compilation
- State management through central `Niri` struct in `src/niri.rs`

**Performance Considerations:**
- Damage tracking for efficient rendering
- Tracy profiler integration for performance analysis
- Multi-threaded property testing with `rayon`
- GPU rendering with fallback to software rendering

## Fork-Specific Features

**Input Inhibition:**
This fork adds an input inhibition feature via IPC action `InhibitInput { inhibited: bool }`:
- Blocks most input processing when enabled (keyboard, pointer, touch, gestures)
- Preserves system-level functionality (exit dialog, device management)  
- Prevents keybinding execution and client input forwarding
- Designed for remote desktop and VM control scenarios
- Implementation in `src/input/mod.rs` and `src/niri.rs`