"""本機授權模式；遠端工具無法修改模式或授權根。"""

READ_ONLY = 'read_only'
FULL_CONTROL = 'full_control'
DEVELOPER_CONTROL = 'developer_control'
HOST_CONTROL = 'host_control'
MODES = (READ_ONLY, FULL_CONTROL, DEVELOPER_CONTROL, HOST_CONTROL)
MODE_LABELS = {
    READ_ONLY: '唯讀模式',
    FULL_CONTROL: '完整控制模式（沙箱 Terminal）',
    DEVELOPER_CONTROL: '開發控制模式（隔離 VM／唯讀工具鏈）',
    HOST_CONTROL: '主機控制模式（受信任命令）',
}
CONTROL_TOOLS = (
    'write_file', 'edit_block', 'create_directory', 'move_file', 'delete_file',
    'start_process', 'read_process_output', 'interact_with_process',
    'list_sessions', 'force_terminate', 'close_session', 'export_session_file',
)
HOST_TOOLS = CONTROL_TOOLS + ('read_host_file', 'list_host_directory')


def normalize_access_mode(value: str = READ_ONLY) -> str:
    if not isinstance(value, str) or value not in MODES:
        raise ValueError('權限模式無效；必須為 read_only、full_control、developer_control 或 host_control。')
    return value


def require_full_control(mode: str) -> None:
    if normalize_access_mode(mode) != FULL_CONTROL:
        raise ValueError('READ_ONLY_MODE：唯讀模式不允許修改檔案或執行命令。')


def control_tools(mode: str) -> tuple[str, ...]:
    mode = normalize_access_mode(mode)
    if mode == READ_ONLY:
        return ()
    return HOST_TOOLS if mode == HOST_CONTROL else CONTROL_TOOLS
