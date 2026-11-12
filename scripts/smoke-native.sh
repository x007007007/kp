#!/bin/bash
# kp native 引擎冒烟测试（scripts/smoke-native.sh）
# 用法：scripts/smoke-native.sh   （自动使用沙箱状态目录 .dev/）
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KP="$ROOT/target/debug/kp"
KPM="$ROOT/target/debug/kpm"
D="$ROOT/.dev"
export XDG_STATE_HOME="$D/state" XDG_RUNTIME_DIR="$D/runtime" XDG_CONFIG_HOME="$D/config"
export KP_LANG=zh   # 断言依赖中文 UI 文案（如"会话退出"摘要），显式锁定语言

fail() { echo "✗ $*"; cleanup; exit 1; }
ok() { echo "✓ $*"; }
cleanup() {
  pkill -9 -f "kp --daemon" 2>/dev/null
  pkill -9 -f "kp _holder" 2>/dev/null
  pkill -9 -f "kp _attach" 2>/dev/null
  pkill -9 -f "kp sleep" 2>/dev/null
  pkill -9 -f "kp -n smoke-" 2>/dev/null
  tmux -L kp kill-server 2>/dev/null
}
trap cleanup EXIT

cleanup
sleep 0.5
rm -rf "$D/state" "$D/runtime" "$D/config"
mkdir -p "$D/state" "$D/runtime" "$D/config"
mkdir -p "$D/state/kp"; : > "$D/state/kp/daemon.log"

echo "── 1. 启动（native 默认引擎）+ 快速命令"
"$KP" echo hi >/dev/null 2>&1 &
sleep 1
"$KP" -n smoke-sleep sleep 120 >/tmp/kp-smoke-client.log 2>&1 &
sleep 2
# 显式命名选会话：不依赖 ls 顺序（快速退出的未命名会话可能因退出上报丢失而残留 running）
NAME="$($KPM ls | awk '$2=="running" && /smoke-sleep/{print $1; exit}')"
[ -n "$NAME" ] || fail "没有 running 会话"
ok "会话 $NAME"

echo "── 2. attach A/B（可写/只读降级）"
"$KP" _attach --socket "$D/runtime/kp/sessions/$NAME.sock" </dev/null >/tmp/kp-smoke-a.log 2>&1 & A=$!
sleep 1
"$KP" _attach --socket "$D/runtime/kp/sessions/$NAME.sock" </dev/null >/tmp/kp-smoke-b.log 2>&1 & B=$!
sleep 1.5
# kp 前台 client 自带 1 个 attach（write）+ 脚本 A（read）+ B（read）= 3
CLIENTS=$(python3 -c "import json; print(json.load(open('$D/runtime/kp/sessions/$NAME.json'))['clients'])")
[ "$CLIENTS" = "3" ] || fail "clients=${CLIENTS}（应 3：client 自带 + A + B）"
ROLES=$(grep -o "role=[a-z]*" "$D/state/kp/daemon.log" | paste -sd, -)
[ "$ROLES" = "role=write,role=read,role=read" ] || fail "授权序列: $ROLES（应 write,read,read）"
ok "clients=3（client+A+B），授权序列 write,read,read"

echo "── 3. Ctrl-C（SIGINT 到子进程组）→ 双端 130"
PGID=$(python3 -c "import json; print(json.load(open('$D/runtime/kp/sessions/$NAME.json'))['pgid'])")
kill -INT -- "-$PGID"; sleep 3
# 终态从 registry 读（holder 状态文件在 exited 清理后由 registry 承载）
$KPM status "$NAME" | grep -q "130" || fail "registry 未记录退出码 130"
$KPM status "$NAME" | grep -q "exited" || fail "registry 状态非 exited"
wait "$A"; AC=$?
wait "$B"; BC=$?
[ "$BC" = "130" ] || fail "B 退出码 ${BC}（应 130）"
grep -qa "会话退出" /tmp/kp-smoke-client.log || fail "client 没有退出摘要"
grep -qa "服务已重启" /tmp/kp-smoke-client.log /tmp/kp-smoke-a.log /tmp/kp-smoke-b.log && true # restart 场景才出现，此处不要求
ok "Ctrl-C → exited 130，B exit=130"

echo "── 4. daemon kill -9 → holder 独立存活 → reconcile"
"$KP" -n smoke-sleep2 sleep 120 >/dev/null 2>&1 &
sleep 2
NAME2="$($KPM ls | awk '$2=="running" && /smoke-sleep2/{print $1; exit}')"
kill -9 "$(pgrep -f 'kp --daemon' | head -1)" 2>/dev/null
sleep 0.5
HOLDERS=$(pgrep -f "kp _holder" | wc -l | tr -d ' ')
[ "$HOLDERS" -ge 1 ] || fail "daemon 死后 holder 应存活"
$KPM ls | grep -q "$NAME2" || fail "reconcile 未认领会话"
ok "holder 存活 $HOLDERS 个，reconcile 认领 $NAME2"

echo "── 5. restart 跟随（无 attach 端时的快速验证）"
$KPM restart "$NAME2" >/dev/null 2>&1 &
sleep 3
$KPM status "$NAME2" 2>/dev/null | grep -q "running" || fail "restart 后应 running"
ok "restart 完成"

echo "── 6. stop 整组零残留"
$KPM stop --all >/dev/null 2>&1
sleep 1
LEFT=$(pgrep -x sleep | wc -l | tr -d ' ')
[ "$LEFT" = "0" ] || fail "sleep 残留 $LEFT"
ok "零残留"

echo "── 7. 双引擎共存（tmux 引擎会话）"
mkdir -p /tmp/kp-engine-test && cd /tmp/kp-engine-test
printf '[run]\nengine = "tmux"\n' > .kp.toml
"$KP" sleep 60 >/dev/null 2>&1 &
sleep 2
$KPM ls | awk '$2=="running"' | grep -q "tmux" || fail "tmux 引擎会话未出现"
$KPM ls | awk '$2=="running"' | grep -q "native" && HAS_NATIVE=1 || HAS_NATIVE=0
ok "tmux 引擎会话已建（native 存活会话: ${HAS_NATIVE}）"
cd /tmp && rm -rf /tmp/kp-engine-test

echo "── 8. 键盘输入经 holder 转发到 PTY（python 作 writer）"
"$KP" -n smoke-read sh -c 'read line; echo "GOT:$line"' >/tmp/kp-smoke-c.log 2>&1 &
sleep 2
NAME3="$($KPM ls | awk '$2=="running" && /smoke-read/{print $1; exit}')"
[ -n "$NAME3" ] || fail "输入测试会话未创建"
pkill -9 -f "kp -n smoke-read" 2>/dev/null; pkill -9 -f "kp _attach" 2>/dev/null
sleep 1
python3 - "$D" "$NAME3" <<'PYEOF2' || fail "输入转发失败"
import socket, json, time, select, sys
d, name = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX)
sockname = "s-" + __import__("hashlib").sha256(name.encode()).hexdigest()[:12] + ".sock"
s.connect(f"{d}/runtime/kp/sessions/{sockname}")
hello = b'A' + json.dumps({"readonly": False, "cols": 80, "rows": 24}).encode()
s.sendall(len(hello).to_bytes(4, "big") + hello)

def recv_frame(timeout=5):
    r, _, _ = select.select([s], [], [], timeout)
    if not r:
        return ("TIMEOUT", b"")
    buf = b""
    while len(buf) < 4:
        c = s.recv(4 - len(buf))
        if not c:
            return ("EOF", b"")
        buf += c
    ln = int.from_bytes(buf, "big")
    while len(buf) < 4 + ln:
        c = s.recv(4 + ln - len(buf))
        if not c:
            return ("EOF", b"")
        buf += c
    return (buf[4:5].decode("latin1"), buf[5:4 + ln])

t, p = recv_frame()
assert t == "W" and json.loads(p)["role"] == "write", f"应为 write 授权: {t} {p}"
time.sleep(0.2)
line = b"FORWARD-OK\n"
s.sendall((len(line) + 1).to_bytes(4, "big") + b"I" + line)
got = False
while True:
    t, p = recv_frame()
    if t == "D" and b"GOT:FORWARD-OK" in p:
        got = True
    elif t == "X":
        assert json.loads(p)["code"] == 0, f"退出码 {p}"
        break
    elif t in ("TIMEOUT", "EOF"):
        raise AssertionError(f"等待超时/EOF: {t}")
assert got, "PTY 未回显输入"
print("✓ 键盘输入经 holder 转发到 PTY 并回显")
PYEOF2

echo "── 9. Ctrl-b 前缀键（pty 端到端）：C-b C-b 字面透传 / C-b d 脱离 / 写锁释放"
$KPM stop --all >/dev/null 2>&1; $KPM clean >/dev/null 2>&1; sleep 1
"$KP" -n smoke-prefix sh -c 'cat' >/dev/null 2>&1 &
sleep 2
NAME4="$($KPM ls | awk '$2=="running" && /smoke-prefix/{print $1; exit}')"
[ -n "$NAME4" ] || fail "前缀测试会话未创建"
pkill -9 -f "kp -n smoke-prefix" 2>/dev/null; pkill -9 -f "kp _attach" 2>/dev/null
sleep 1
python3 - "$KP" "$D" "$NAME4" <<'PYEOF3' || fail "前缀键测试失败"
import pty, os, subprocess, time, select, socket, json, sys
kp, d, name = sys.argv[1], sys.argv[2], sys.argv[3]
master, slave = pty.openpty()
p = subprocess.Popen([kp, "_attach", "--socket", f"{d}/runtime/kp/sessions/{name}.sock"],
                     stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
time.sleep(1)

def read_avail(timeout):
    out = b""
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([master], [], [], 0.2)
        if r:
            out += os.read(master, 65536)
    return out

# 9a. C-b C-b → 转发恰好一个字面 C-b；对端 PTY echo（ECHOCTL）回显 ^B（或原始 0x02）各一次
os.write(master, b"\x02\x02")
echo = read_avail(1.5)
n = echo.count(b"^B") + echo.count(b"\x02")
assert n == 1, f"字面 C-b 应恰好转发一次，实际 {n} 次（echo={echo!r}）"

# 9b. C-b d → detach：_attach 以 0 退出（脱离消息由外层 client 打印）
os.write(master, b"\x02d")
for _ in range(50):
    if p.poll() is not None:
        break
    time.sleep(0.1)
assert p.poll() == 0, f"detach 后 _attach 退出码 {p.poll()}（应 0）"

# 9c. detach 断连即释放写锁：新写客户端应获得 write 授权
s = socket.socket(socket.AF_UNIX)
sockname = "s-" + __import__("hashlib").sha256(name.encode()).hexdigest()[:12] + ".sock"
s.connect(f"{d}/runtime/kp/sessions/{sockname}")
hello = b'A' + json.dumps({"readonly": False, "cols": 80, "rows": 24}).encode()
s.sendall(len(hello).to_bytes(4, "big") + hello)
buf = b""
while len(buf) < 4:
    c = s.recv(4 - len(buf))
    assert c, "握手 EOF"
    buf += c
ln = int.from_bytes(buf, "big")
while len(buf) < 4 + ln:
    buf += s.recv(4 + ln - len(buf))
assert buf[4:5] == b"W" and json.loads(buf[5:])["role"] == "write", f"应 write 授权: {buf[:40]}"
s.close()
print("✓ C-b C-b 字面透传 / C-b d 脱离（exit 0）/ 写锁已释放")
PYEOF3
$KPM status "$NAME4" 2>/dev/null | grep -q "running" || fail "detach 后会话应仍 running"
ok "detach 后会话 $NAME4 仍 running"

$KPM stop --all >/dev/null 2>&1; $KPM clean >/dev/null 2>&1
echo ""
echo "═══ 全部通过 ═══"
