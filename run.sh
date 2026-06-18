#!/bin/bash
# Script to compile, watch, and run the GetAIBD VS Code extension

EXTENSION_PATH="/Users/abuhaidersiddiq/codes/vscode-getaibd-plugin/getaibd"

echo "Building the GetAIBD VS Code extension..."
cd "$EXTENSION_PATH" || exit 1

# Ensure dependencies are installed
if [ ! -d "node_modules" ]; then
    echo "node_modules not found, running npm install..."
    npm install
fi

# Run an initial clean compile
npm run compile

echo "Starting watch task in the background..."
# Start the esbuild watch compiler in the background
npm run watch:esbuild > /dev/null 2>&1 &
WATCH_PID=$!

# Ensure the background watch process is stopped when the script exits
cleanup() {
    echo "Stopping background watch task (PID: $WATCH_PID)..."
    kill "$WATCH_PID" 2>/dev/null
}
trap cleanup EXIT

echo "Launching VS Code in extension development host..."
if command -v code >/dev/null 2>&1; then
    # --wait blocks the terminal until the launched VS Code window is closed
    code --extensionDevelopmentPath="$EXTENSION_PATH" --wait
else
    echo "Warning: 'code' command-line tool not found in PATH."
    echo "Please open VS Code, open the Command Palette (Cmd+Shift+P),"
    echo "search for 'Shell Command: Install 'code' command in PATH', run it, and retry."
    exit 1
fi
