"""Windows 登入入口：啟動主程式至系統匣，沿用既有排程。"""


def main() -> int:
    from local_files_gui import main as run_gui
    return run_gui(startup=True)


if __name__ == '__main__':
    raise SystemExit(main())
