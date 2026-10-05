"""Sanitized diagnostics from the live registry, not the advertised tool list."""
import hashlib
import json
from importlib.metadata import version
from pathlib import Path

import incremental_state as state
import operation_budget
from image_reader import image_limits
from format_reader import format_limits
from reader_settings import DEFAULT_EXCLUSIONS
from tool_contract import SERVICE_VERSION, CONTRACT_VERSION, contract, contract_filename


def canonical_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=True,
                                    separators=(',', ':')).encode()).hexdigest()


def diagnose(workspace, declared):
    from workspace_reader import effective_tool_limits
    from access_mode import DEVELOPER_CONTROL, HOST_CONTROL, FULL_CONTROL
    developer_status = None
    if workspace._access_mode == DEVELOPER_CONTROL:
        from developer_control import backend_status
        developer_status = backend_status(getattr(workspace, '_developer_toolchains', []))
    provider = workspace._registered_tools_provider
    actual = contract(provider()) if provider is not None else {}
    expected = {}
    contract_state = 'ok'
    try:
        path = Path(__file__).with_name(contract_filename(workspace._access_mode))
        with path.open('rb') as handle:
            data = handle.read(2 * 1024 * 1024 + 1)
        if len(data) > 2 * 1024 * 1024:
            raise ValueError()
        expected = json.loads(data)
        if not isinstance(expected, dict) or any(not isinstance(v, dict) for v in expected.values()):
            raise ValueError()
    except (OSError, ValueError):
        expected = {}
        contract_state = 'unavailable_or_invalid'
    version_provider = workspace._registered_service_version_provider
    server_version_matches = (version_provider is not None and version_provider() == SERVICE_VERSION)
    declared_names, actual_names, contract_names = set(declared), set(actual), set(expected)
    # Expose only known program identifiers, not arbitrary names injected into a contract file.
    mismatches = sorted(name for name in declared_names & actual_names & contract_names
                        if actual[name] != expected[name])
    consistent = (provider is not None and server_version_matches and contract_state == 'ok' and
                  declared_names == actual_names == contract_names and not mismatches)
    roots = []
    for identifier, reader in workspace.readers.items():
        accessible = True
        try:
            workspace.reader(identifier)
        except (ValueError, OSError):
            accessible = False
        limits = dict(reader.settings)
        limits['excluded_names'] = sorted(set(limits['excluded_names']) | DEFAULT_EXCLUSIONS)
        roots.append(dict(id=identifier, accessible=accessible, effective_settings=limits))
    return dict(service_version=SERVICE_VERSION, contract_version=CONTRACT_VERSION,
        access_mode=workspace._access_mode,
        mode_ready=developer_status['ready'] if developer_status else True,
        developer_backend=developer_status,
        mcp_version=version('mcp'), declared_tools=list(declared),
        registered_tools=sorted(actual_names & declared_names),
        unexpected_registered_count=len(actual_names - declared_names),
        registry_attached=provider is not None, consistent=consistent,
        server_version_matches=server_version_matches,
        contract_status=contract_state, schema_or_annotation_mismatches=mismatches,
        missing_registration=sorted(declared_names - actual_names),
        missing_contract=sorted(declared_names - contract_names),
        unexpected_contract_count=len(contract_names - declared_names),
        actual_contract_sha256=canonical_digest(actual),
        expected_contract_sha256=canonical_digest(expected) if contract_state == 'ok' else None,
        image_limits=image_limits(), format_limits=format_limits(),
        parser_versions=dict(pypdf=version('pypdf'), mutagen=version('mutagen'), pillow=version('Pillow'), pyyaml=version('PyYAML')),
        roots=roots, text_tool_limits=effective_tool_limits(), limits=dict(max_workers=operation_budget.MAX_WORKERS,
            operation_timeout_seconds=operation_budget.TIMEOUT_SECONDS,
            max_operation_bytes=1024 * 1024 * 1024, max_operation_entries=300000,
            max_tool_json_bytes=2 * 1024 * 1024, max_hash_files=state.MAX_HASH_FILES,
            max_snapshot_entries=state.MAX_SNAPSHOT_ENTRIES,
            max_snapshot_bytes=state.SNAPSHOT_BYTES, max_baseline_cache_bytes=state.CACHE_BYTES,
            max_baselines=state.MAX_BASELINES, max_owner_baselines=state.MAX_OWNER_BASELINES,
            baseline_ttl_seconds=state.BASELINE_TTL_SECONDS, max_change_items=state.MAX_CHANGE_ITEMS),
        capabilities=dict(sha256='sha256' in hashlib.algorithms_available,
            formats='all normal files: metadata/hash/raw; UTF-8 text; static PNG/JPEG/WebP/GIF/BMP/TIFF/ICO/PNM and AVIF when available; PDF/DOCX/XLSX/PPTX/ODT/ODS/ODP/EPUB/EML/MBOX; CSV/TSV/JSON/JSONL/XML/YAML/INI/SRT/VTT; TOML/SQLite runtime-gated; MP3/FLAC/Ogg/WAV/MP4/AVI/AIFF/AU/Matroska/WebM; ZIP/TAR/TAR.GZ directory and bounded member bytes',
            raw_bytes_hash=True, metadata_reuse=True, force_hash=True,
            metadata_identity='device/inode/size/mtime/ctime/links/attributes; Windows handle ChangeTime',
            snapshots='bounded process memory only; expire on restart, TTL or eviction',
            source_read_only=workspace._access_mode == 'read_only', atomic_filesystem_snapshot=False,
            terminal={FULL_CONTROL: 'windows_appcontainer', HOST_CONTROL: 'host_process_trusted',
                      DEVELOPER_CONTROL: 'windows_sandbox_vm'}.get(workspace._access_mode, 'disabled'),
            command_execution=workspace._access_mode in (FULL_CONTROL, HOST_CONTROL) or bool(developer_status and developer_status['ready']),
            host_commands_sandboxed=False if workspace._access_mode == HOST_CONTROL else None,
            client_tool_discovery='not_observable_by_server',
            client_verification='Compare client tools/list with registered_tools and contract digest; then call tools.'))
