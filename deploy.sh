#!/usr/bin/env bash
set -euo pipefail

echo "Building solcast-proxy (release)..."
cargo build --release

echo "Installing binary..."
sudo cp target/release/solcast-proxy /usr/local/bin/

echo "Creating cache directory..."
sudo mkdir -p /var/lib/solcast-proxy

echo "Installing systemd service..."
sudo cp solcast-proxy.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now solcast-proxy

echo "Done. Status:"
sudo systemctl status solcast-proxy --no-pager
