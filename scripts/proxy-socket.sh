#!/usr/bin/env bash

PC_SOCKET_PATH="${COPILOT_PROXY_RS_PC_SOCKET-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.process-compose/pc.sock}"
export PC_SOCKET_PATH
