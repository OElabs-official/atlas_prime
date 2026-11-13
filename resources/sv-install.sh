#!/data/data/com.termux/files/usr/bin/sh
# atlas_prime runit 一键安装/卸载（服务资产随本脚本同目录：sv/atlas_prime/）
# 用法: sv-install.sh install|uninstall
# 前提: termux-services 已安装（提供 $SVDIR 与 sv-enable/sv-disable）
set -eu

SV_NAME="atlas_prime"

usage() {
	printf 'Usage: %s install|uninstall\n' "$0" >&2
	exit 2
}

guard_svdir() {
	if [ -z "${SVDIR:-}" ]; then
		printf 'Error: SVDIR 未设置——pkg install termux-services 后开新 Termux 会话\n' >&2
		exit 2
	fi
}

[ "$#" -eq 1 ] || usage
case "$1" in
	install)
		guard_svdir
		cd "$(dirname "$0")"
		mkdir -p "$SVDIR/$SV_NAME/log"
		cp "sv/$SV_NAME/run" "$SVDIR/$SV_NAME/run"
		cp "sv/$SV_NAME/log/run" "$SVDIR/$SV_NAME/log/run"
		chmod 700 "$SVDIR/$SV_NAME" "$SVDIR/$SV_NAME/log"
		chmod 700 "$SVDIR/$SV_NAME/run" "$SVDIR/$SV_NAME/log/run"
		# svlogd 不会自建日志目录，缺目录时 log/run 直接退出（log 服务崩溃循环）
		LOG_ROOT="${LOGDIR:-/data/data/com.termux/files/usr/var/log}"
		mkdir -p "$LOG_ROOT/sv/$SV_NAME"
		chmod 700 "$LOG_ROOT/sv/$SV_NAME"
		# 装而不启：无 down 占位时 runsvdir ≤5s 即自动拉起，绕开"显式 enable"决策
		touch "$SVDIR/$SV_NAME/down"
		chmod 600 "$SVDIR/$SV_NAME/down"
		echo "Installed (disabled): $SVDIR/$SV_NAME"
		echo "Next steps:"
		echo "  1) sv-enable $SV_NAME"
		echo "  2) Termux:Boot autostart: run termux-wake-lock once, then put"
		echo "     'source \$PREFIX/etc/profile.d/start-services.sh'"
		echo "     into a script under ~/.termux/boot/ (e.g. ~/.termux/boot/start-services.sh)"
		echo "  note: 请退役旧的手动拉起方式——flock 单例会把双启动记为一次 crash。"
		;;
	uninstall)
		guard_svdir
		sv-disable "$SV_NAME" 2>/dev/null || true
		rm -rf "$SVDIR/$SV_NAME"
		echo "Uninstalled: $SVDIR/$SV_NAME"
		;;
	*)
		usage
		;;
esac
