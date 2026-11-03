"""消费产品 owner 的固定 SQL 与状态合同，保留部署写动作的单次执行边界"""
import hashlib
import json
from pathlib import Path
import re
import subprocess


FILES = {'contract': 'contract.json', 'inspect': 'inspect.sql', 'prepare': 'prebuild.sql',
         'cleanup': 'cleanup-invalid.sql', 'rollback': 'revert.sql'}
ROLES = tuple(FILES)
BUNDLE = Path(__file__).resolve().parent / 'schema53-contract'


def require(ok, code):
    if not ok:
        raise RuntimeError(code)


def contract(value):
    require(isinstance(value, dict) and
            re.fullmatch('[0-9a-f]{40}', value.get('source', '')) and
            isinstance(value.get('evidence'), str) and value['evidence'].strip() and
            isinstance(value.get('artifacts'), dict) and set(value['artifacts']) == set(ROLES),
            'schema53_product_contract_required')
    for role, artifact in value['artifacts'].items():
        require(isinstance(artifact, dict) and artifact.get('name') == FILES[role] and
                re.fullmatch('[0-9a-f]{64}', artifact.get('sha256', '')),
                'schema53_exact_artifact_identity_required')
    return value


def artifact_bytes(directory, value, role):
    item = contract(value)['artifacts'][role]
    data = (Path(directory) / item['name']).read_bytes()
    # 原文工件经 manifest 绑定，跨机投递或替换必须重新核对身份
    require(hashlib.sha256(data).hexdigest() == item['sha256'], 'schema53_artifact_drift_' + role)
    return data


def product_contract(directory, value):
    result = json.loads(artifact_bytes(directory, value, 'contract'))
    require(result.get('contract') == 'cpr-successful-trace-index/2' and result.get('target_schema') == 53 and
            result.get('product_candidate') == value['source'], 'schema53_product_contract_version_mismatch')
    return result


def inspect(directory, value, query):
    product = product_contract(directory, value)
    data = artifact_bytes(directory, value, 'inspect')
    result = json.loads(query(data.decode('utf-8')))
    require(isinstance(result, dict) and result.get('contract') == product['contract'] and
            result.get('schema_state') in product['actions']['inspect']['success']['schema_state'] and
            result.get('index_state') in product['actions']['inspect']['success']['index_state'] and
            isinstance(result.get('active_operations'), list), 'schema53_inspection_invalid')
    return result


def validate_observation(value, version, pending_required=False):
    require(value.get('schema_state') == 'schema' + str(version) and
            value.get('schema_version') == version, 'schema53_product_ledger_mismatch')
    require(value.get('active_operations') == [], 'schema53_original_database_action_still_active')
    require(value.get('index_state') == 'valid' if pending_required else
            value.get('index_state') in ('missing', 'valid', 'invalid', 'drop_incomplete'),
            'schema53_pending_index_not_ready')


def require_quiescent(root, value, query):
    result = inspect(Path(root) / 'schema53-artifacts', value, query)
    require(result['active_operations'] == [], 'schema53_original_database_action_still_active')
    return result


def execute(directory, value, role, pg, timeout):
    require(role in ('prepare', 'cleanup', 'rollback'), 'schema53_write_role_invalid')
    product = product_contract(directory, value)
    data = artifact_bytes(directory, value, role)
    argv = ['docker', 'exec', '-i', '-u', 'postgres', '-e',
            'PGOPTIONS=-c default_transaction_read_only=off', pg, 'sh', '-c',
            'exec psql -X -w -qAt -U "$POSTGRES_USER" -d codex_proxy -v ON_ERROR_STOP=1 -v VERBOSITY=sqlstate']
    try:
        result = subprocess.run(argv, input=data, capture_output=True, timeout=timeout)
    except (subprocess.TimeoutExpired, OSError):
        raise RuntimeError('schema53_' + role + '_result_unknown_no_reissue') from None
    # 只有服务器明确 SQLSTATE 才能把失败归为已知；Docker/连接错误维持未知
    if result.returncode != 0:
        state = re.search(rb'^(?:ERROR|FATAL):[ \t]+([0-9A-Z]{5})(?::|\r?$)', result.stderr, re.MULTILINE)
        if not state or state[1].decode() not in product['errors']:
            raise RuntimeError('schema53_' + role + '_result_unknown_no_reissue')
        return {'outcome': 'failed', 'sqlstate': state[1].decode(), 'returnCode': result.returncode}
    try:
        observed = json.loads(result.stdout)
        expected = product['actions'][{'prepare': 'prebuild', 'cleanup': 'cleanup-invalid', 'rollback': 'revert'}[role]]['success']
        require(observed.get('contract') == product['contract'] and
                all(observed.get(key) == item for key, item in expected.items()) and
                observed.get('active_operations') == [], 'schema53_action_postcondition_invalid')
    except (ValueError, AttributeError, RuntimeError):
        raise RuntimeError('schema53_' + role + '_result_unknown_no_reissue') from None
    return {'outcome': 'completed', 'returnCode': 0, 'product': observed}
