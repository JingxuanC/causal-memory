#!/usr/bin/env bash
# =============================================================================
# install.sh — causal-memory 引导式安装器
#
# 把 causal-memory 从「一堆 skill 文件」变成「agent 可直接调用的记忆」。
# 完整链路: skill 文件 + 服务端二进制 + MCP server 注册 —— 三者齐了,
# 重启 agent 会话后 record_decision / search_memory 等 17 个工具即可用。
# 本脚本随 skill 一起发布(位于 SKILL.md 旁边); 仓库根另有一个转发 wrapper。
#
# 做的事:
#   0. 检测环境(服务端二进制 / pip / 各 agent 客户端)
#   1. 安装 skill:软链到 ~/.agents/skills/ 与 ~/.claude/skills/(仓库为单一源)
#   2. 安装服务端:pip install causal-memory(若二进制缺失)
#   3. 注册 MCP server 到检测到的客户端:
#        Claude Code   → claude mcp add -s <scope>
#        Cursor / Claude Desktop → mcp.json 合并 mcpServers
#        Opencode      → config.json 合并 mcp
#        Kimi Code     → config.toml 追加 [mcp.servers.causal-memory]
#   4. 验证 + 重启提示
#
# 用法:
#   ./install.sh                      交互式引导安装
#   ./install.sh --yes                全自动, 接受所有默认
#   ./install.sh --copy               用复制代替软链(兼容不识别软链的环境)
#   ./install.sh --skip-server        跳过服务端安装
#   ./install.sh --skip-mcp           跳过 MCP 注册
#   ./install.sh --clients claude,opencode   只注册指定客户端(逗号分隔)
#   ./install.sh --scope user         Claude Code MCP 作用域: local|project|user
#   ./install.sh --uninstall          卸载(skill + 可选 MCP)
#   ./install.sh --help               打印本说明
# =============================================================================
set -euo pipefail

SKILL="causal-memory"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# 源 skill 目录自适应两种布局:
#   - 仓库布局: install.sh 在仓库根, skill 在 skills/<name>/  (git clone 后)
#   - 已打包布局: install.sh 就在 skill 目录内                  (SkillsHub 安装后)
if [ -d "$SCRIPT_DIR/skills/$SKILL" ]; then
  SKILL_SRC="$SCRIPT_DIR/skills/$SKILL"
else
  SKILL_SRC="$SCRIPT_DIR"
fi
AGENTS_SKILLS_DIR="${AGENTS_SKILLS_DIR:-$HOME/.agents/skills}"
CLAUDE_SKILLS_DIR="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"
# 备份目录必须在 skills 目录之外:skills/*/SKILL.md 会被 agent 当 skill 扫描,
# 备份留在里面会污染 skill 列表。
BACKUP_DIR="$HOME/.skill-backups"

# --- 各客户端配置文件路径(先探测, 不存在则给默认目标) ---
CURSOR_CFG="$HOME/.cursor/mcp.json"
OPENCODE_CFG=""
for f in "$HOME/.config/opencode/config.json" "$HOME/.config/opencode/opencode.json" "$HOME/.config/opencode/opencode.jsonc"; do
  [ -f "$f" ] && { OPENCODE_CFG="$f"; break; }
done
[ -z "$OPENCODE_CFG" ] && OPENCODE_CFG="$HOME/.config/opencode/config.json"
DESKTOP_CFG=""
for f in "$HOME/Library/Application Support/Claude/claude_desktop_config.json" \
         "$HOME/.config/Claude/claude_desktop_config.json"; do
  [ -f "$f" ] && { DESKTOP_CFG="$f"; break; }
done
KIMI_CFG=""
for f in "$HOME/.config/kimi/config.toml" "$HOME/.kimi/config.toml"; do
  [ -f "$f" ] && { KIMI_CFG="$f"; break; }
done

# --- 选项 ---
MODE="link"          # link | copy
ACTION="install"     # install | uninstall
YES=0                # 全自动
SKIP_SERVER=0
SKIP_MCP=0
CLAUDE_SCOPE="user"  # Claude Code MCP 作用域(全局, 每个会话都可用)
CLIENTS_FILTER=""    # 逗号分隔, 空=全部
while [ $# -gt 0 ]; do
  case "$1" in
    --copy) MODE="copy" ;;
    --uninstall) ACTION="uninstall" ;;
    --yes|-y) YES=1 ;;
    --skip-server) SKIP_SERVER=1 ;;
    --skip-mcp) SKIP_MCP=1 ;;
    --scope) CLAUDE_SCOPE="${2:-user}"; shift ;;
    --scope=*) CLAUDE_SCOPE="${1#--scope=}" ;;
    --clients) CLIENTS_FILTER="${2:-}"; shift ;;
    --clients=*) CLIENTS_FILTER="${1#--clients=}" ;;
    -h|--help) sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d'; exit 0 ;;
    *) echo "未知参数: $1(用 --help 查看用法)" >&2; exit 1 ;;
  esac
  shift
done

# --- 彩色输出(仅 TTY) ---
if [ -t 1 ]; then
  BOLD=$'\033[1m'; GRN=$'\033[32m'; YEL=$'\033[33m'; CYN=$'\033[36m'; RED=$'\033[31m'; RST=$'\033[0m'
else
  BOLD=""; GRN=""; YEL=""; CYN=""; RED=""; RST=""
fi

ok()   { printf '%s✓%s %s\n' "$GRN" "$RST" "$*"; }
info() { printf '%s›%s %s\n' "$CYN" "$RST" "$*"; }
warn() { printf '%s!%s %s\n' "$YEL" "$RST" "$*"; }
fail() { printf '%s✗%s %s\n' "$RED" "$RST" "$*"; }
step() { printf '\n%s%s%s\n' "$BOLD" "$1" "$RST"; }

# --- 交互确认:$1=提示, $2=默认(y/n);返回 0=y,1=n ---
confirm() {
  local prompt="$1" default="${2:-y}"
  if [ "$YES" = 1 ] || [ ! -t 0 ]; then { [ "$default" = y ] && return 0 || return 1; }; fi
  local hint; [ "$default" = y ] && hint="Y/n" || hint="y/N"
  printf '%s [%s] ' "$prompt" "$hint"
  local ans; IFS= read -r ans; ans="${ans:-$default}"
  case "$ans" in y|Y|yes|YES|Yes) return 0 ;; *) return 1 ;; esac
}

# --- 检测 ---
binary_path()  { command -v causal-memory 2>/dev/null || true; }
pip_path()     { command -v pip 2>/dev/null || command -v pip3 2>/dev/null || command -v pipx 2>/dev/null || true; }
binary_version() {
  (pip show causal-memory 2>/dev/null || pip3 show causal-memory 2>/dev/null) \
    | awk '/^Version:/{print $2; exit}'
}

client_label() {
  case "$1" in
    claude) echo "Claude Code" ;;
    cursor) echo "Cursor" ;;
    opencode) echo "Opencode" ;;
    desktop) echo "Claude Desktop" ;;
    kimi) echo "Kimi Code" ;;
  esac
}

client_detected() {
  case "$1" in
    claude)   command -v claude >/dev/null 2>&1 ;;
    cursor)   [ -d "$HOME/.cursor" ] ;;
    opencode) [ -f "$OPENCODE_CFG" ] ;;
    desktop)  [ -n "$DESKTOP_CFG" ] && [ -f "$DESKTOP_CFG" ] ;;
    kimi)     [ -n "$KIMI_CFG" ] && [ -f "$KIMI_CFG" ] ;;
    *) return 1 ;;
  esac
}

# --- JSON 合并 / 移除(用 python3, 稳健合并、保留已有键、支持 jsonc 注释) ---
json_merge() {  # $1=file $2=top-key $3=server-name $4=value-json → echo EXISTS|UPDATED|ERROR
  local file="$1" key="$2" name="$3" val="$4"
  python3 - "$file" "$key" "$name" "$val" <<'PY'
import json, sys, os
f, key, name, val = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
def strip_jsonc(s):
    out=[]; i=0; n=len(s); instr=False; esc=False
    while i<n:
        ch=s[i]
        if instr:
            out.append(ch)
            if esc: esc=False
            elif ch=='\\': esc=True
            elif ch=='"': instr=False
            i+=1; continue
        if ch=='"': instr=True; out.append(ch); i+=1; continue
        if ch=='/' and i+1<n and s[i+1]=='/':
            while i<n and s[i]!='\n': i+=1
            continue
        if ch=='/' and i+1<n and s[i+1]=='*':
            i+=2
            while i+1<n and not (s[i]=='*' and s[i+1]=='/'): i+=1
            i+=2; continue
        out.append(ch); i+=1
    return ''.join(out)
data={}
if os.path.exists(f):
    try:
        data=json.loads(strip_jsonc(open(f).read()))
    except Exception as e:
        print("ERROR: 无法解析 %s: %s" % (f, e)); sys.exit(0)
node=data.setdefault(key,{})
if not isinstance(node,dict):
    print("ERROR: %s 中 %s 不是对象" % (f, key)); sys.exit(0)
v=json.loads(val)
if node.get(name)==v:
    print("EXISTS"); sys.exit(0)
node[name]=v
os.makedirs(os.path.dirname(f) or '.', exist_ok=True)
open(f,'w').write(json.dumps(data,indent=2,ensure_ascii=False)+'\n')
print("UPDATED")
PY
}

json_remove() {  # $1=file $2=top-key $3=server-name → echo REMOVED|ABSENT
  local file="$1" key="$2" name="$3"
  [ -f "$file" ] || { echo "ABSENT"; return 0; }
  python3 - "$file" "$key" "$name" <<'PY'
import json, sys, os
f, key, name = sys.argv[1], sys.argv[2], sys.argv[3]
data=json.load(open(f))
node=data.get(key,{})
if isinstance(node,dict) and name in node:
    del node[name]
    open(f,'w').write(json.dumps(data,indent=2,ensure_ascii=False)+'\n')
    print("REMOVED")
else:
    print("ABSENT")
PY
}

# --- skill 安装/卸载 ---
install_skill() {
  local skills_dir="$1" src="$SKILL_SRC" dst="$1/$SKILL"
  mkdir -p "$skills_dir"
  # 目标已指向源(同名软链) 或 源本身就在目标里(从已装位置运行 install.sh 的自链情形) → 跳过, 幂等
  if [ -e "$dst" ] && [ -d "$src" ] && [ "$(cd "$src" && pwd -P)" = "$(cd "$dst" && pwd -P)" ]; then
    echo "  · 已就位: $dst"
    return 0
  fi
  if [ "$MODE" = "link" ]; then
    if [ -e "$dst" ] && [ ! -L "$dst" ]; then
      mkdir -p "$BACKUP_DIR"
      mv "$dst" "$BACKUP_DIR/$SKILL.bak.$(date +%s)"
      echo "  ↩  已备份旧文件: $dst → $BACKUP_DIR/"
    fi
    ln -sfn "$src" "$dst"   # -n: 目标已是目录软链时不进入其内部
    echo "  🔗 skill 软链: $dst → $src"
  else
    rm -rf "$dst"; cp -R "$src" "$dst"
    echo "  📄 skill 复制: $dst"
  fi
}

uninstall_skill() {
  local dst="$1/$SKILL"
  if [ -L "$dst" ] || [ -d "$dst" ]; then rm -rf "$dst"; echo "  🗑  移除: $dst"; fi
}

# --- 服务端安装 ---
server_install() {
  if [ -n "$(binary_path)" ]; then
    ok "服务端已安装: $(binary_path) v$(binary_version)"
    return 0
  fi
  local PIP; PIP="$(pip_path)"
  if [ -z "$PIP" ]; then
    fail "未找到 pip/pip3/pipx, 无法自动安装。请手动: pip install causal-memory"
    return 1
  fi
  info "安装服务端: $PIP install causal-memory"
  "$PIP" install causal-memory
  ok "服务端安装完成: $(binary_path)"
}

# --- MCP 注册 ---
register_client() {  # $1=client → echo 状态描述
  local c="$1"
  case "$c" in
    claude)
      if claude mcp add -s "$CLAUDE_SCOPE" "$SKILL" -- causal-memory >/dev/null 2>&1; then
        echo "已注册 (scope=$CLAUDE_SCOPE)"
      else
        echo "注册失败(请确认 claude CLI 可用)"
      fi ;;
    cursor)   json_merge "$CURSOR_CFG"  mcpServers "$SKILL" '{"command":"causal-memory"}' ;;
    opencode) json_merge "$OPENCODE_CFG" mcp          "$SKILL" '{"type":"local","command":"causal-memory","enabled":true}' ;;
    desktop)  json_merge "$DESKTOP_CFG"  mcpServers "$SKILL" '{"command":"causal-memory"}' ;;
    kimi)     kimi_register "$KIMI_CFG" ;;
  esac
}

kimi_register() {  # $1=config.toml → echo EXISTS|UPDATED
  local cfg="$1"
  [ -f "$cfg" ] || { echo "ERROR: 未找到 config.toml"; return 0; }
  if grep -q '^\[mcp\.servers\.causal-memory\]' "$cfg"; then echo "EXISTS"; return 0; fi
  printf '\n[mcp.servers.causal-memory]\ncommand = "causal-memory"\n' >> "$cfg"
  echo "UPDATED"
}

unregister_client() {  # $1=client → echo 状态
  local c="$1"
  case "$c" in
    claude)
      if claude mcp remove -s "$CLAUDE_SCOPE" "$SKILL" >/dev/null 2>&1; then
        echo "已移除"; else echo "未注册/移除失败"; fi ;;
    cursor)   json_remove "$CURSOR_CFG"  mcpServers "$SKILL" ;;
    opencode) json_remove "$OPENCODE_CFG" mcp          "$SKILL" ;;
    desktop)  json_remove "$DESKTOP_CFG"  mcpServers "$SKILL" ;;
    kimi)     { grep -v -e '^\[mcp\.servers\.causal-memory\]$' -e '^command = "causal-memory"$' "$KIMI_CFG" > "$KIMI_CFG.tmp" 2>/dev/null && mv "$KIMI_CFG.tmp" "$KIMI_CFG" && echo "已移除"; } || echo "ABSENT" ;;
  esac
}

# 返回需要处理的客户端列表(每行一个), 受 CLIENTS_FILTER 约束
target_clients() {
  local c
  for c in claude cursor opencode desktop kimi; do
    if [ -n "$CLIENTS_FILTER" ]; then
      case ",$CLIENTS_FILTER," in *",$c,"*) ;; *) continue ;; esac
    fi
    client_detected "$c" && echo "$c"
  done
  return 0   # 循环末尾 client_detected 可能返回 1, 避免 set -e 误触发
}

# --- 打印 JSON 注册结果的人话 ---
print_json_status() {
  case "$1" in
    EXISTS) warn "已存在, 跳过" ;;
    UPDATED) ok "已注册到 $2" ;;
    ERROR*) fail "$1" ;;
  esac
}

# =============================================================================
# 主流程
# =============================================================================
echo "${BOLD}=== causal-memory 引导式安装 [$ACTION] ===${RST}"

if [ "$ACTION" = uninstall ]; then
  step "卸载 skill"
  uninstall_skill "$AGENTS_SKILLS_DIR"
  [ -d "$HOME/.claude" ] && uninstall_skill "$CLAUDE_SKILLS_DIR"

  if [ "$SKIP_MCP" = 0 ] && confirm "同时移除各客户端的 MCP 注册?" y; then
    step "移除 MCP 注册"
    for c in $(target_clients); do
      printf '  %-16s → %s\n' "$(client_label "$c")" "$(unregister_client "$c")"
    done
  fi
  echo
  ok "卸载完成。"
  exit 0
fi

# ---- 0. 环境检测 ----
step "0/4 · 环境检测"
if [ -n "$(binary_path)" ]; then
  ok "服务端 binary: $(binary_path) v$(binary_version)"
else
  warn "服务端 binary 未找到(稍后安装)"
fi
[ -n "$(pip_path)" ] && ok "包管理器: $(pip_path)" || warn "未找到 pip/pip3/pipx"

DETECTED="$(target_clients)"
echo -n "  agent 客户端: "
if [ -z "$DETECTED" ]; then
  echo "(未检测到, 仅装 skill + 服务端)"
else
  echo
  for c in $DETECTED; do echo "    - $(client_label "$c")"; done
fi

# ---- 1. skill ----
step "1/4 · 安装 skill"
if confirm "软链 skill 到 $AGENTS_SKILLS_DIR${CLAUDE_SKILLS_DIR:+ 与 $CLAUDE_SKILLS_DIR}?" y; then
  install_skill "$AGENTS_SKILLS_DIR"
  if [ -d "$HOME/.claude" ]; then
    install_skill "$CLAUDE_SKILLS_DIR"
  fi
else
  warn "跳过 skill 安装"
fi

# ---- 2. 服务端 ----
step "2/4 · 安装服务端"
if [ "$SKIP_SERVER" = 1 ]; then
  warn "跳过(--skip-server)"
elif confirm "安装服务端 binary (causal-memory CLI + MCP server)?" y; then
  server_install || warn "服务端未就绪, MCP 工具将无法使用"
else
  warn "跳过服务端安装(MCP 工具将无法使用, 除非已装)"
fi

# ---- 3. MCP 注册 ----
step "3/4 · 注册 MCP server"
if [ "$SKIP_MCP" = 1 ]; then
  warn "跳过(--skip-mcp)"
elif [ -z "$DETECTED" ]; then
  warn "未检测到客户端, 请按 SKILL.md §1 手动注册"
else
  for c in $DETECTED; do
    if ! confirm "注册到 $(client_label "$c")?" y; then
      warn "跳过 $(client_label "$c")"
      continue
    fi
    st="$(register_client "$c" || echo 'ERROR: 注册失败')"
    case "$c" in
      cursor|opencode|desktop) print_json_status "$st" "$(client_label "$c")" ;;
      kimi) [ "$st" = EXISTS ] && warn "已存在, 跳过" || ok "已注册到 $(client_label "$c")" ;;
      claude) ok "Claude Code MCP: $st" ;;
    esac
  done
fi

# ---- 4. 验证 + 收尾 ----
step "4/4 · 验证"
BIN="$(binary_path)"
if [ -n "$BIN" ]; then
  if "$BIN" stats >/dev/null 2>&1; then
    ok "causal-memory stats 可读(存储正常)"
  else
    warn "causal-memory stats 异常(可能是首次运行, 或 DB 权限)"
  fi
else
  fail "未检测到 causal-memory 二进制 —— 请先完成步骤 2"
fi

echo
echo "${BOLD}✅ 完成。${RST}"
echo
echo "  接下来:"
echo "    1. ${BOLD}重启 agent 会话${RST}(MCP server 在启动时加载)"
echo "    2. 验证工具可用:"
if command -v claude >/dev/null 2>&1; then
  echo "       claude mcp list        # 应显示 causal-memory ✔ Connected"
fi
echo "       或在会话里直接调 ${CYN}causal_directory${RST} / ${CYN}search_memory${RST}(空目录=正常, 报错=server 没起来)"
echo "    3. 之后按 SKILL.md §2 主动 recall/record(决策前查、行动后记)"
