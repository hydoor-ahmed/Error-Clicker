# 🖱️ Error404: High-Performance Auto Clicker
**A Stealthy, Wayland-Native Auto Clicker built with Rust.** 🦀

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-Linux%20(Wayland/X11)-blue.svg)](https://archlinux.org/)
[![Status](https://img.shields.io/badge/status-Stable-green.svg)]()

**Error404** is a specialized auto-clicker designed for Linux environments (optimized for **Manjaro/Arch**). It bypasses **Wayland's** strict input isolation by leveraging the **XDG Desktop Portal** and **evdev** sniffing, making it both powerful and stealthy for security research and accessibility.

---

## ✨ Features

* **Native Wayland Support**: Uses the `Enigo` portal-based injection to bypass compositor restrictions without emulation lags.
* **Zero-Latency Sniffing**: Directly listens to `/dev/input/` events for near-instant response times.
* **Humanized Jitter**: Implements a dynamic randomization algorithm (Entropy-based) to fluctuate CPS (Clicks Per Second) and avoid pattern detection.
* **Modern GUI**: A sleek, Dark-mode interface built with `egui` for real-time CPS adjustment and engine control.
* **Atomic Engine**: Built on Rust's concurrency primitives (`Arc`, `AtomicBool`, `AtomicU64`) for high-performance, thread-safe execution.

---

## 🛠️ Tech Stack

- **Language:** [Rust](https://www.rust-lang.org/) 🦀
- **GUI Framework:** [egui / eframe](https://github.com/emilk/egui)
- **Input Handling:** [evdev-rs](https://github.com/ndreynolds/evdev-rs) & [Enigo](https://github.com/enigo-rs/enigo)
- **Entropy & Randomization:** [rand](https://github.com/rust-random/rand) (v0.9)

---

## 🚀 Getting Started

### Prerequisites
Ensure you have the Rust toolchain installed and the necessary portal libraries for Wayland.

```bash
# For Arch/Manjaro
sudo pacman -S --needed base-devel libx11 libxtst libportal-gtk3
