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
# Pre-load PATHs for tools installed under $HOME. rustup and bun both write
# their PATH update to ~/.bashrc / ~/.zshrc, which only takes effect for new
# shells — not for this script's subshell or any re-run from the same parent.
# Pulling these in explicitly makes the presence check idempotent.
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
[[ -d "$HOME/.bun/bin" ]] && export PATH="$HOME/.bun/bin:$PATH"

MISSING_APT=()
command -v git    &>/dev/null || MISSING_APT+=(git)
command -v unzip  &>/dev/null || MISSING_APT+=(unzip)     # bun installer requires unzip
command -v cc     &>/dev/null || MISSING_APT+=(build-essential)  # Rust crates with C deps need a linker

if [[ ${#MISSING_APT[@]} -gt 0 ]]; then
    case "$OS" in
        Linux)
            error "Missing system packages: ${MISSING_APT[*]}\n  Install them first:\n    sudo apt-get update && sudo apt-get install -y ${MISSING_APT[*]}"
            ;;
        Darwin)
            error "Missing tools: ${MISSING_APT[*]}\n  On macOS these usually come from Xcode CLI tools:\n    xcode-select --install"
            ;;
    esac
fi

# fetch_and_run <url> <expected_sha256_or_empty> <label> <args...>
# Downloads a remote installer to a temp file, prints its SHA-256 so you can
# pin it on the next run, optionally verifies against an expected hash, then
# executes it with the given args. Replaces the classic `curl ... | sh`,
# which executes arbitrary code with no integrity check.
fetch_and_run() {
    local url="$1"; shift
    local expected="$1"; shift
    local label="$1"; shift

    local tmp
    tmp="$(mktemp -t "forge-${label}-installer.XXXXXX")"
    trap "rm -f \"$tmp\"" RETURN

    info "Downloading $label installer from $url"
    if ! curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$tmp"; then
        error "Failed to download $label installer from $url"
    fi

    local actual
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$tmp" | awk '{print $1}')"
    else
        warn "No sha256sum/shasum found — cannot verify $label installer integrity"
        actual=""
    fi

    if [[ -n "$actual" ]]; then
        info "$label installer SHA-256: $actual"
    fi

    if [[ -n "$expected" ]]; then
        if [[ -z "$actual" ]]; then
            error "Cannot verify $label installer: no sha256 tool available"
        fi
        if [[ "$actual" != "$expected" ]]; then
            error "$label installer SHA-256 mismatch.\n  expected: $expected\n  got:      $actual\n  Refusing to execute."
        fi
        ok "$label installer SHA-256 verified"
    else
        warn "$label installer not pinned — to pin, re-run with FORGE_${label^^}_SHA256=$actual"
    fi

    bash "$tmp" "$@"
}

if ! command -v cargo &>/dev/null; then
    warn "Rust not found — installing via rustup..."
    fetch_and_run "https://sh.rustup.rs" "${FORGE_RUSTUP_SHA256:-}" "rustup" -y
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    ok "Rust installed"
else
    ok "Rust found: $(rustc --version)"
fi

if ! command -v bun &>/dev/null; then
    warn "Bun not found — installing..."
    fetch_and_run "https://bun.sh/install" "${FORGE_BUN_SHA256:-}" "bun"
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
# Stamp the install with the source SHA if we're in a git checkout — non-fatal for tarball installs.
git rev-parse HEAD 2>/dev/null > "$INSTALL_SHARE/version" || echo "unknown" > "$INSTALL_SHARE/version"

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
PATH_RELOAD_HINT=""
if [[ ":$PATH:" != *":$INSTALL_BIN:"* ]]; then
    SHELL_NAME="$(basename "$SHELL")"
    case "$SHELL_NAME" in
        zsh)  RC_FILE="$HOME/.zshrc" ;;
        bash) RC_FILE="$HOME/.bashrc" ;;
        fish) RC_FILE="$HOME/.config/fish/config.fish" ;;
        *)    RC_FILE="" ;;
    esac

    if [[ -n "$RC_FILE" ]]; then
        if ! grep -qF "$INSTALL_BIN" "$RC_FILE" 2>/dev/null; then
            mkdir -p "$(dirname "$RC_FILE")"
            echo "" >> "$RC_FILE"
            echo "# Added by Forge installer" >> "$RC_FILE"
            if [[ "$SHELL_NAME" == "fish" ]]; then
                echo "set -gx PATH $INSTALL_BIN \$PATH" >> "$RC_FILE"
            else
                echo "export PATH=\"$INSTALL_BIN:\$PATH\"" >> "$RC_FILE"
            fi
            ok "Added $INSTALL_BIN to PATH in $RC_FILE"
        else
            warn "$INSTALL_BIN is already in $RC_FILE but not loaded in this shell."
        fi
        PATH_RELOAD_HINT="source $RC_FILE   # or open a new terminal"
    else
        PATH_RELOAD_HINT="export PATH=\"$INSTALL_BIN:\$PATH\"   # add to your shell's rc file to make permanent"
    fi

    # Export for this subshell so the post-install steps (wizard, OAuth login,
    # connection test) can find forge-agent without restarting.
    export PATH="$INSTALL_BIN:$PATH"
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
    echo -e "${BOLD}Configure Forge${RESET}"
    echo "Choose how you want Forge to reach an LLM."
    echo ""

    # Helper — read a non-empty value, looping until the user types something.
    prompt_required() {
        # $1 = prompt text, $2 = variable name to write into
        local __prompt="$1" __var="$2" __value=""
        while [[ -z "$__value" ]]; do
            read -r -p "$__prompt" __value
            [[ -z "$__value" ]] && echo "    Required."
        done
        printf -v "$__var" "%s" "$__value"
    }

    # Top-level: what kind of LLM access?
    echo "  1) Local LLM server   (LM Studio, Ollama, llama.cpp, vLLM, etc.) — runs fully offline"
    echo "  2) Claude subscription   (claude.ai / Pro / Max — OAuth login)"
    echo "  3) ChatGPT Codex subscription   (OAuth login)"
    echo "  4) Direct API key   (Anthropic API, OpenAI API, OpenRouter, custom OpenAI-compatible)"
    echo "  5) Skip — I'll edit the config file myself"
    echo ""

    LLM_CHOICE=""
    while [[ ! "$LLM_CHOICE" =~ ^[1-5]$ ]]; do
        read -r -p "  Choice [1-5]: " LLM_CHOICE
    done
    echo ""

    mkdir -p "$CONFIG_DIR"
    POST_INSTALL_HINT=""

    case "$LLM_CHOICE" in
        1)
            # ---------- Local LLM server ----------
            echo "  Examples:"
            echo "    LM Studio default base URL:  http://127.0.0.1:1234/v1"
            echo "    Ollama default base URL:     http://127.0.0.1:11434/v1"
            echo "    llama.cpp server default:    http://127.0.0.1:8080/v1"
            echo ""

            prompt_required "    Endpoint name (any label you'll recognize): " EP_NAME
            prompt_required "    Base URL: " EP_URL
            echo "    Model ID — the exact identifier your server expects."
            echo "    Tip: enter \"auto\" to have Forge query /v1/models on startup."
            prompt_required "    Model ID: " EP_MODEL
            prompt_required "    Context window in tokens (e.g. 32768, 131072): " EP_CONTEXT
            EP_CONTEXT="${EP_CONTEXT//,/}"   # tolerate "131,072"

            cat > "$CONFIG_FILE" << CONFIG
[models]
default = "$EP_NAME"

[[models.endpoints]]
name = "$EP_NAME"
base_url = "$EP_URL"
model_id = "$EP_MODEL"
max_context_tokens = $EP_CONTEXT
max_output_tokens = 8192
endpoint_type = "open_ai"
CONFIG
            ;;

        2)
            # ---------- Claude subscription ----------
            cat > "$CONFIG_FILE" << CONFIG
[models]
default = "claude"

[[models.endpoints]]
name = "claude"
base_url = "https://api.anthropic.com"
model_id = "auto"
max_context_tokens = 200000
max_output_tokens = 8192
endpoint_type = "anthropic"
CONFIG
            LOGIN_CMD="--login"
            LOGIN_LABEL="Claude"
            LOGIN_PORT="53692"
            ;;

        3)
            # ---------- ChatGPT Codex subscription ----------
            cat > "$CONFIG_FILE" << CONFIG
[models]
default = "chatgpt-codex"

[[models.endpoints]]
name = "chatgpt-codex"
base_url = "https://chatgpt.com/backend-api/codex"
model_id = "auto"
max_context_tokens = 200000
max_output_tokens = 16384
endpoint_type = "chatgpt_codex"
CONFIG
            LOGIN_CMD="--login-chatgpt"
            LOGIN_LABEL="ChatGPT Codex"
            LOGIN_PORT="1455"
            ;;

        4)
            # ---------- Direct API key ----------
            echo "  Which provider?"
            echo "    1) Anthropic API     (api.anthropic.com — keys start with sk-ant-)"
            echo "    2) OpenAI API        (api.openai.com — keys start with sk-)"
            echo "    3) OpenRouter        (openrouter.ai/api)"
            echo "    4) Custom OpenAI-compatible endpoint"
            echo ""
            API_PROVIDER=""
            while [[ ! "$API_PROVIDER" =~ ^[1-4]$ ]]; do
                read -r -p "    Provider [1-4]: " API_PROVIDER
            done

            case "$API_PROVIDER" in
                1) EP_URL="https://api.anthropic.com"; EP_TYPE="anthropic"; EP_NAME="anthropic" ;;
                2) EP_URL="https://api.openai.com/v1"; EP_TYPE="open_ai"; EP_NAME="openai" ;;
                3) EP_URL="https://openrouter.ai/api/v1"; EP_TYPE="open_ai"; EP_NAME="openrouter" ;;
                4)
                    prompt_required "    Endpoint name (any label): " EP_NAME
                    prompt_required "    Base URL: " EP_URL
                    EP_TYPE="open_ai"
                    ;;
            esac

            prompt_required "    API key: " EP_KEY
            prompt_required "    Model ID (e.g. claude-sonnet-4-6, gpt-4o, anthropic/claude-opus-4): " EP_MODEL
            prompt_required "    Context window in tokens (e.g. 200000): " EP_CONTEXT
            EP_CONTEXT="${EP_CONTEXT//,/}"

            cat > "$CONFIG_FILE" << CONFIG
[models]
default = "$EP_NAME"

[[models.endpoints]]
name = "$EP_NAME"
base_url = "$EP_URL"
model_id = "$EP_MODEL"
api_key = "$EP_KEY"
max_context_tokens = $EP_CONTEXT
max_output_tokens = 8192
endpoint_type = "$EP_TYPE"
CONFIG
            ;;

        5)
            # ---------- Skip ----------
            cat > "$CONFIG_FILE" << CONFIG
# Forge config — edit this file to point at your LLM, then run "forge".
#
# Examples:
#
# Local OpenAI-compatible server (LM Studio, Ollama, llama.cpp, vLLM):
#
#   [models]
#   default = "local"
#
#   [[models.endpoints]]
#   name = "local"
#   base_url = "http://127.0.0.1:1234/v1"
#   model_id = "auto"
#   max_context_tokens = 32768
#   max_output_tokens = 8192
#   endpoint_type = "open_ai"
#
# Claude / ChatGPT subscription — set endpoint_type to "anthropic" or
# "chatgpt_codex" with no api_key, then run "forge-agent --login" or
# "forge-agent --login-chatgpt" to authenticate via OAuth.
#
# Direct API key — add api_key = "..." to the endpoint block.
#
# See ARCHITECTURE.md for the full config reference.

[models]
default = "placeholder"

[[models.endpoints]]
name = "placeholder"
base_url = "http://127.0.0.1:1234/v1"
model_id = "auto"
max_context_tokens = 32768
max_output_tokens = 8192
endpoint_type = "open_ai"
CONFIG
            POST_INSTALL_HINT="Edit the config file before running forge:\n    $CONFIG_FILE\n  The file is annotated with examples for local servers, OAuth subscriptions, and direct API keys.\n  Or re-run ./install.sh to use the interactive wizard."
            ;;
    esac

    # Common agent block — same for every choice.
    DEFAULT_MODEL=$(awk -F\" '/^default *=/ {print $2; exit}' "$CONFIG_FILE")
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
default_model = "$DEFAULT_MODEL"
CONFIG

    ok "Config written to $CONFIG_FILE"

    # OAuth login flow for subscription choices (2, 3).
    # Offer to run the login inline; warn first if we're on a remote SSH session
    # since the OAuth callback hits localhost:$LOGIN_PORT (needs port forwarding).
    if [[ "$LLM_CHOICE" == "2" || "$LLM_CHOICE" == "3" ]]; then
        echo ""
        if [[ -n "${SSH_CONNECTION:-}" ]]; then
            warn "You're on a remote SSH session. The $LOGIN_LABEL OAuth flow opens a browser"
            warn "and waits for a callback on port $LOGIN_PORT — which only works if you've already"
            warn "forwarded that port from your local machine. To forward it, exit and reconnect with:"
            warn "    ssh -L $LOGIN_PORT:localhost:$LOGIN_PORT <user>@<host>"
            warn "Then re-run:  forge-agent $LOGIN_CMD"
            echo ""
            read -r -p "Try running login now anyway? [y/N]: " RUN_LOGIN
            [[ "$RUN_LOGIN" =~ ^[Yy] ]] && DO_LOGIN=1 || DO_LOGIN=0
        else
            read -r -p "Run $LOGIN_LABEL OAuth login now? [Y/n]: " RUN_LOGIN
            [[ "$RUN_LOGIN" =~ ^[Nn] ]] && DO_LOGIN=0 || DO_LOGIN=1
        fi

        if [[ "$DO_LOGIN" == "1" ]]; then
            echo ""
            info "Launching $LOGIN_LABEL OAuth login..."
            if "$INSTALL_BIN/forge-agent" "$LOGIN_CMD"; then
                ok "Logged in to $LOGIN_LABEL"
            else
                warn "Login did not complete. You can retry later with:  forge-agent $LOGIN_CMD"
                POST_INSTALL_HINT="To authenticate later, run:  forge-agent $LOGIN_CMD"
            fi
        else
            POST_INSTALL_HINT="To authenticate later, run:  forge-agent $LOGIN_CMD"
        fi
    fi

    # Connection test only makes sense for choices 1 and 4.
    if [[ "$LLM_CHOICE" == "1" || "$LLM_CHOICE" == "4" ]]; then
        echo ""
        read -r -p "Test the endpoint now? [y/N]: " TEST_CONN
        if [[ "$TEST_CONN" =~ ^[Yy] ]]; then
            if curl -sf --max-time 5 "${EP_URL}/models" > /dev/null 2>&1 || \
               curl -sf --max-time 5 "${EP_URL%/v1}/v1/models" > /dev/null 2>&1; then
                ok "Endpoint reachable: $EP_URL"
            else
                warn "Could not reach $EP_URL — start your server, then run forge."
            fi
        fi
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

if [[ -n "$PATH_RELOAD_HINT" ]]; then
    echo ""
    echo -e "  ${BOLD}To run \`forge\` in this terminal right now:${RESET}"
    echo "    $PATH_RELOAD_HINT"
fi

if [[ -n "${POST_INSTALL_HINT:-}" ]]; then
    echo ""
    echo -e "  ${BOLD}Next step:${RESET}"
    # shellcheck disable=SC2059
    echo -e "  $POST_INSTALL_HINT"
fi
echo ""
