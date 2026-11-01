#!/bin/sh
# kp 提交前秘密扫描：检查暂存文件中的疑似敏感信息。
# 启用方式：git config core.hooksPath hooks
# 用法：scripts/check-secrets.sh [--all]  （--all 扫描全部跟踪文件而非仅暂存区）
set -u

status=0
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

if [ "${1:-}" = "--all" ]; then
    files="$(git ls-files)"
else
    files="$(git diff --cached --name-only --diff-filter=ACMR)"
fi

check() {
    pat="$1"
    label="$2"
    printf '%s\n' "$files" | while IFS= read -r f; do
        [ -n "$f" ] || continue
        [ -f "$f" ] || continue
        # -I 跳过二进制文件
        if LC_ALL=C grep -nIE "$pat" "$f" >"$tmp" 2>/dev/null; then
            echo "✗ 疑似${label} —— $f"
            sed 's/^/    /' "$tmp"
            echo "    （如为误报，请改写代码使其不匹配，或在提交说明中说明理由后 git commit --no-verify）"
            echo "FAIL"
        fi
    done | grep -q FAIL && status=1
}

# 私钥 / 云厂商 / 平台令牌
check '-----BEGIN [A-Z ]*PRIVATE KEY'          '私钥块'
check 'AKIA[0-9A-Z]{16}'                       'AWS AccessKey'
check 'github_pat_[A-Za-z0-9_]{20,}'           'GitHub fine-grained token'
check 'gh[posu]_[A-Za-z0-9]{36}'               'GitHub token'
check 'xox[baprs]-[A-Za-z0-9-]{10,}'           'Slack token'
check '(^|[^A-Za-z])sk-[A-Za-z0-9]{20,}'       'API secret key（sk-…）'
check 'AIza[0-9A-Za-z_-]{35}'                  'Google API key'
# 通用赋值形态
check '(api[_-]?key|secret|passwd|password|token|credential)[\"'"'"' ]*[:=][\"'"'"' ]*[A-Za-z0-9+/_=-]{16,}' '通用密钥赋值'
# 隐私：绝对用户路径（泄露机器用户名）
check '/Users/[a-z0-9_.-]+/|/home/[a-z0-9_.-]+/' '本机绝对路径'

if [ "$status" -eq 0 ]; then
    echo "✓ 秘密扫描通过"
fi
exit "$status"
