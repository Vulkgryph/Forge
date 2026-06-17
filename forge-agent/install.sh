#!/usr/bin/env bash
set -euo pipefail

# Forge installer — builds from source and installs to ~/.local/
# Usage: ./install.sh

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

INSTALL_BIN="$HOME/.local/bin"
INSTALL_SHARE="$HOME/.local/share/forge"

info()  { echo -e "${BOLD}==>${RESET} $1"; }
warn()  { echo -e "${YELLOW}warning:${RESET} $1"; }
error() { echo -e "${RED}error:${RESET} $1" >&2; exit 1; }
ok()    { echo -e "${GREEN}✓${RESET} $1"; }

# -------------------------------------------------------------------
# 1. Detect OS/arch
# -------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) ;;
    Linux)  ;;
    *)      error "Unsupported OS: $OS (only macOS and Linux are supported)" ;;
esac

info "Detected $OS $ARCH"

# -------------------------------------------------------------------
# 2. Check system dependencies
# -------------------------------------------------------------------
if ! command -v git &>/dev/null; then
    error "git is required but not installed. Install it first:\n  macOS: xcode-select --install\n  Linux: sudo apt install git"
fi

if ! command -v cargo &>/dev/null; then
    warn "Rust not found — installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    ok "Rust installed"
else
    ok "Rust found: $(rustc --version)"
fi

if ! command -v bun &>/dev/null; then
    warn "Bun not found — installing..."
    curl -fsSL https://bun.sh/install | bash
    export PATH="$HOME/.bun/bin:$PATH"
    ok "Bun installed"
else
    ok "Bun found: $(bun --version)"
fi

# -------------------------------------------------------------------
# 3. Build
# -------------------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO_ROOT"

info "Building forge-agent (Rust)..."
cargo build --release
ok "forge-agent built"

info "Building UI (Bun)..."
(cd ui && bun install && bun run build)
ok "UI built"

# -------------------------------------------------------------------
# 4. Install
# -------------------------------------------------------------------
info "Installing to $INSTALL_BIN and $INSTALL_SHARE..."

mkdir -p "$INSTALL_BIN" "$INSTALL_SHARE/ui/dist"

ln -sf "$(pwd)/target/release/forge-agent" "$INSTALL_BIN/forge-agent"
ln -sf "$(pwd)/ui/dist/forge.js" "$INSTALL_SHARE/ui/dist/forge.js"
ln -sf "$(pwd)/update.sh" "$INSTALL_BIN/forge-update"
cp -r ui/node_modules          "$INSTALL_SHARE/ui/node_modules"
cp ui/package.json             "$INSTALL_SHARE/ui/package.json"
git rev-parse HEAD           > "$INSTALL_SHARE/version"

# Write wrapper script
cat > "$INSTALL_BIN/forge" << 'WRAPPER'
#!/usr/bin/env bash
FORGE_HOME="$HOME/.local/share/forge"
exec bun run "$FORGE_HOME/ui/dist/forge.js" "$@"
WRAPPER

chmod +x "$INSTALL_BIN/forge" "$INSTALL_BIN/forge-agent" "$INSTALL_BIN/forge-update" update.sh

# Remove wrappers from the pre-rename "sinter" install if they are still present.
rm -f "$INSTALL_BIN/sinter" "$INSTALL_BIN/sinter-agent" "$INSTALL_BIN/sinter-update"

ok "Installed forge → $INSTALL_BIN/forge"
ok "Installed forge-agent → $INSTALL_BIN/forge-agent"
ok "Installed forge-update → $INSTALL_BIN/forge-update"

# -------------------------------------------------------------------
# 5. PATH setup
# -------------------------------------------------------------------
if [[ ":$PATH:" != *":$INSTALL_BIN:"* ]]; then
    SHELL_NAME="$(basename "$SHELL")"
    case "$SHELL_NAME" in
        zsh)  RC_FILE="$HOME/.zshrc" ;;
        bash) RC_FILE="$HOME/.bashrc" ;;
        *)    RC_FILE="" ;;
    esac

    if [[ -n "$RC_FILE" ]]; then
        if ! grep -qF "$INSTALL_BIN" "$RC_FILE" 2>/dev/null; then
            echo "" >> "$RC_FILE"
            echo "# Added by Forge installer" >> "$RC_FILE"
            echo "export PATH=\"$INSTALL_BIN:\$PATH\"" >> "$RC_FILE"
            warn "Added $INSTALL_BIN to PATH in $RC_FILE"
            warn "Run: source $RC_FILE  (or open a new terminal)"
        fi
    else
        warn "$INSTALL_BIN is not in your PATH. Add it manually:\n  export PATH=\"$INSTALL_BIN:\$PATH\""
    fi
fi

# -------------------------------------------------------------------
# 6. Config
# -------------------------------------------------------------------
CONFIG_DIR="$HOME/.config/forge"
CONFIG_FILE="$CONFIG_DIR/config.toml"
LEGACY_CONFIG_DIR="$HOME/.config/sinter"

if [[ ! -d "$CONFIG_DIR" && -d "$LEGACY_CONFIG_DIR" ]]; then
    mkdir -p "$(dirname "$CONFIG_DIR")"
    cp -R "$LEGACY_CONFIG_DIR" "$CONFIG_DIR"
    ok "Migrated existing config to $CONFIG_DIR"
fi

if [[ -f "$CONFIG_FILE" ]]; then
    # Config exists — update it in place to ensure all current fields are present.
    # Preserves all user values (endpoints, models, agent settings) while adding
    # any new fields introduced since the config was first written.
    info "Updating existing config at $CONFIG_FILE..."
    python3 - "$CONFIG_FILE" << 'PYEOF'
import sys, re

path = sys.argv[1]
with open(path) as f:
    content = f.read()

def ensure_field(content, section_header, key, default_value):
    """Add key=value after section_header if key is not already present."""
    # Check if the key already exists anywhere (simple check)
    if re.search(rf'^\s*{re.escape(key)}\s*=', content, re.MULTILINE):
        return content
    # Find the section and insert after it
    pattern = rf'({re.escape(section_header)}[^\[]*)'
    def inserter(m):
        block = m.group(1).rstrip()
        return block + f'\n{key} = {default_value}\n'
    new_content = re.sub(pattern, inserter, content, count=1, flags=re.DOTALL)
    return new_content

# Ensure agent section fields
content = ensure_field(content, '[agent]', 'context_strategy', '"compaction"')
content = ensure_field(content, '[agent]', 'thinking_mode', 'false')
content = ensure_field(content, '[agent]', 'disabled_tools', '[]')
content = ensure_field(content, '[agent]', 'permission_mode', '"default"')

# Ensure each endpoint has max_output_tokens and request_timeout_secs
# For endpoints we insert after each [[models.endpoints]] block
def update_endpoints(content):
    # Find each [[models.endpoints]] block and ensure required fields are present.
    # Each block ends at the next [[...]] section header.
    def fix_block(m):
        block = m.group(0)
        if not re.search(r'^\s*max_output_tokens\s*=', block, re.MULTILINE):
            block = block.rstrip('\n') + '\nmax_output_tokens = 16384\n'
        if not re.search(r'^\s*request_timeout_secs\s*=', block, re.MULTILINE):
            block = block.rstrip('\n') + '\nrequest_timeout_secs = 500\n'
        return block
    # Match each endpoint block: from [[models.endpoints]] up to next [ section or end
    return re.sub(
        r'\[\[models\.endpoints\]\].*?(?=\n\[|\Z)',
        fix_block,
        content,
        flags=re.DOTALL
    )

content = update_endpoints(content)

with open(path, 'w') as f:
    f.write(content)

# Extract endpoint names and URLs for connection testing
import json
endpoints = []
for m in re.finditer(r'\[\[models\.endpoints\]\](.*?)(?=\n\[|\Z)', content, re.DOTALL):
    block = m.group(1)
    name  = re.search(r'^\s*name\s*=\s*"([^"]+)"', block, re.MULTILINE)
    url   = re.search(r'^\s*base_url\s*=\s*"([^"]+)"', block, re.MULTILINE)
    if name and url:
        endpoints.append((name.group(1), url.group(1)))

print("Config updated.")
print(json.dumps(endpoints))
PYEOF
    ok "Config updated at $CONFIG_FILE"

    # Ping each existing endpoint
    ENDPOINTS_JSON=$(python3 - "$CONFIG_FILE" << 'PYEOF2'
import sys, re, json
content = open(sys.argv[1]).read()
endpoints = []
for m in re.finditer(r'\[\[models\.endpoints\]\](.*?)(?=\n\[|\Z)', content, re.DOTALL):
    block = m.group(1)
    name = re.search(r'^\s*name\s*=\s*"([^"]+)"', block, re.MULTILINE)
    url  = re.search(r'^\s*base_url\s*=\s*"([^"]+)"', block, re.MULTILINE)
    if name and url: endpoints.append([name.group(1), url.group(1)])
print(json.dumps(endpoints))
PYEOF2
)
    echo ""
    echo -e "  ${BOLD}Testing existing endpoints...${RESET}"
    python3 -c "import json; eps=json.loads('$ENDPOINTS_JSON'); [print(e[0]+'|'+e[1]) for e in eps]" | \
    while IFS='|' read -r EP_NAME EP_URL; do
        if curl -sf --max-time 5 "${EP_URL}/models" > /dev/null 2>&1 || \
           curl -sf --max-time 5 "${EP_URL}/v1/models" > /dev/null 2>&1; then
            ok "${EP_NAME}: connected (${EP_URL})"
        else
            warn "${EP_NAME}: could not reach ${EP_URL} — make sure your server is running"
        fi
    done
else
    echo ""
    echo -e "${BOLD}Configure your LLM endpoints${RESET}"
    echo "Forge works with any OpenAI-compatible server (LM Studio, llama.cpp, Ollama, vLLM, etc.)"
    echo ""

    EP_NAMES=()
    EP_URLS=()
    EP_MODELS=()
    EP_CONTEXTS=()

    # Collect one or more endpoints
    FIRST=true
    while true; do
        if [[ "$FIRST" == true ]]; then
            echo -e "  ${BOLD}Endpoint 1${RESET} (this will be your default)"
        else
            IDX=$(( ${#EP_NAMES[@]} + 1 ))
            echo -e "  ${BOLD}Endpoint $IDX${RESET}"
        fi

        DEFAULT_NAME="local"
        [[ "$FIRST" == false ]] && DEFAULT_NAME="model-$(( ${#EP_NAMES[@]} + 1 ))"
        read -r -p "    Name [$DEFAULT_NAME]: " EP_NAME
        EP_NAME="${EP_NAME:-$DEFAULT_NAME}"

        DEFAULT_URL="http://127.0.0.1:1234/v1"
        read -r -p "    URL (press Enter to accept default) [$DEFAULT_URL]: " EP_URL
        EP_URL="${EP_URL:-$DEFAULT_URL}"

        EP_MODEL=""
        while [[ -z "$EP_MODEL" ]]; do
            read -r -p "    Model ID (e.g. qwen2.5-coder-32b): " EP_MODEL
            [[ -z "$EP_MODEL" ]] && echo "    Model ID is required."
        done

        read -r -p "    Context window size [32768]: " EP_CONTEXT
        EP_CONTEXT="${EP_CONTEXT:-32768}"
        EP_CONTEXT="${EP_CONTEXT//,/}"  # strip commas (e.g. 32,768 → 32768)

        EP_NAMES+=("$EP_NAME")
        EP_URLS+=("$EP_URL")
        EP_MODELS+=("$EP_MODEL")
        EP_CONTEXTS+=("$EP_CONTEXT")
        FIRST=false

        echo ""
        read -r -p "  Add another endpoint? [y/N]: " ADD_MORE
        ADD_MORE="${ADD_MORE:-N}"
        echo ""
        [[ "$ADD_MORE" =~ ^[Yy] ]] || break
    done

    mkdir -p "$CONFIG_DIR"

    # Write config header
    cat > "$CONFIG_FILE" << CONFIG
[models]
default = "${EP_NAMES[0]}"
CONFIG

    # Write each endpoint
    for i in "${!EP_NAMES[@]}"; do
        cat >> "$CONFIG_FILE" << CONFIG

[[models.endpoints]]
name = "${EP_NAMES[$i]}"
base_url = "${EP_URLS[$i]}"
model_id = "${EP_MODELS[$i]}"
max_context_tokens = ${EP_CONTEXTS[$i]}
max_output_tokens = 8192
CONFIG
    done

    # Write agent config
    cat >> "$CONFIG_FILE" << CONFIG

[agent]
auto_approve_reads = true
auto_approve_writes = false
permission_mode = "default"
max_history_messages = 200
compaction_threshold = 150

[agent.subagents]
enabled = true
max_depth = 2
max_concurrent = 4
default_model = "${EP_NAMES[0]}"
CONFIG

    ok "Config written to $CONFIG_FILE"
    [[ ${#EP_NAMES[@]} -gt 1 ]] && echo "  Tip: use /model inside Forge to switch between your endpoints."

    echo ""
    read -r -p "Test connections now? [Y/n]: " TEST_CONN
    TEST_CONN="${TEST_CONN:-Y}"
    if [[ "$TEST_CONN" =~ ^[Yy] ]]; then
        for i in "${!EP_NAMES[@]}"; do
            if curl -sf --max-time 5 "${EP_URLS[$i]}/models" > /dev/null 2>&1; then
                ok "${EP_NAMES[$i]}: connected to ${EP_URLS[$i]}"
            else
                warn "${EP_NAMES[$i]}: could not reach ${EP_URLS[$i]} — start your LLM server, then run forge."
            fi
        done
    fi
fi

# -------------------------------------------------------------------
# 7. Done
# -------------------------------------------------------------------
echo ""
echo -e "${GREEN}${BOLD}Forge installed successfully!${RESET}"
echo ""
echo "  forge          Launch Forge"
echo "  forge-agent    Launch in headless mode (for scripting)"
echo "  forge-update   Update, rebuild, and reinstall Forge"
echo ""
echo "  Config: $CONFIG_FILE"
echo ""
