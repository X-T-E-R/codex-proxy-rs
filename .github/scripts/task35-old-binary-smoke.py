"""Run the pinned old writer against the recovered synthetic DB on an internal-only network."""
import argparse
import json
from pathlib import Path
import subprocess
import time
from urllib.parse import quote

parser = argparse.ArgumentParser()
parser.add_argument('--artifacts', type=Path, required=True)
parser.add_argument('--binary-directory', type=Path, required=True)
parser.add_argument('--postgres', required=True)
parser.add_argument('--redis', required=True)
parser.add_argument('--restore-dump', action='store_true')
args = parser.parse_args()
receipt_path = args.artifacts / 'task35-recovery.json'
receipt = json.loads(receipt_path.read_text())
schema, database = receipt['recovery_schema'], receipt['restored_database']
binaries = list(args.binary_directory.rglob('codex-proxy-rs'))
assert len(binaries) == 1, binaries
binary = binaries[0].resolve()
binary.chmod(0o755)
network = database + '_internal'
container = database + '_old45'
workspace = (args.artifacts / 'old45-workspace').resolve()
(workspace / 'deploy').mkdir(parents=True)
(workspace / 'web').mkdir()
(workspace / 'runtime').mkdir()
config = {
    'schema_version': 1,
    'host': {'listen': {'host':'0.0.0.0','port':8080},'runtime_data_dir':'/workspace/runtime',
             'logging':{'level':'info','stdout':True,'oauth_recovery':False,'request_dump':False,
                        'request_dump_retention_days':1,
                        'file':{'enabled':False,'directory':'/workspace/runtime/logs',
                                'retention_days':7,'max_file_size_mb':20}},
             'drain_timeout_seconds':2,'worker_shutdown_timeout_seconds':2},
    'store': {'database':{'url':'postgres://codex_proxy@postgres:5432/'+database+'?options='+quote('-csearch_path='+schema,safe=''),
                           'password':'codex_proxy'},
              'redis':{'url':'redis://redis:6379/','password':''}},
    'admin': {'session_ttl_minutes':1440,'default_username':'admin@task35.synthetic',
              'default_password':'task35-synthetic-only'},
    'client':{'session_ttl_minutes':1440},
    'api':{'asset_directory':'/workspace/web','cors_allowed_origins':[],
           'request_timeout_seconds':None,'request_id_header':'x-request-id'},
    'openai':{'residency':'us'},
}
(workspace / 'deploy/config.yaml').write_text(json.dumps(config),encoding='utf-8')

def run(arguments, **kwargs):
    return subprocess.run(arguments,check=True,capture_output=True,**kwargs)


def sql(text):
    result = run(['docker','exec','--env','PGPASSWORD=codex_proxy',args.postgres,
                  'psql','-X','-U','codex_proxy','-d',database,'-At','-v','ON_ERROR_STOP=1','-c',text],text=True)
    return result.stdout.strip()

if args.restore_dump:
    began = time.monotonic()
    run(['docker','exec','--env','PGPASSWORD=codex_proxy',args.postgres,
         'createdb','-U','codex_proxy',database])
    with (args.artifacts / receipt['dump_file']).open('rb') as dump:
        run(['docker','exec','-i','--env','PGPASSWORD=codex_proxy',args.postgres,
             'pg_restore','-U','codex_proxy','-d',database,'--exit-on-error','--no-owner'],stdin=dump)
    receipt['old_binary_dump_replay_seconds'] = round(time.monotonic()-began,3)
began = time.monotonic()
health_status = None
try:
    run(['docker','network','create','--internal',network])
    run(['docker','network','connect','--alias','postgres',network,args.postgres])
    run(['docker','network','connect','--alias','redis',network,args.redis])
    assert json.loads(run(['docker','network','inspect',network],text=True).stdout)[0]['Internal'] is True
    run(['docker','run','--detach','--name',container,'--network',network,
         '--mount',f'type=bind,source={binary},target=/app/codex-proxy-rs,readonly',
         '--mount',f'type=bind,source={workspace},target=/workspace',
         '--workdir','/workspace','--entrypoint','/app/codex-proxy-rs',
         'postgres:18-bookworm@sha256:d9c83446333daec3f0588cc709adb80c26090b7f9f0f7ec8d43c243385d79818','serve'])
    deadline = time.monotonic()+60
    while time.monotonic() < deadline:
        result = subprocess.run(['docker','exec',container,'bash','-c',
            "exec 3<>/dev/tcp/127.0.0.1/8080; printf 'GET /healthz HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n' >&3; head -n 1 <&3"],capture_output=True,text=True)
        if result.returncode == 0 and result.stdout.startswith('HTTP/'):
            health_status = int(result.stdout.split()[1])
            if health_status == 204:
                break
        if run(['docker','inspect','--format','{{.State.Running}}',container],text=True).stdout.strip() != 'true':
            break
        time.sleep(1)
    log = run(['docker','logs',container])
    (args.artifacts / 'old45-startup.log').write_bytes(log.stdout + log.stderr)
    assert health_status in {204,503}, 'old binary did not initialize HTTP; inspect old45-startup.log'
    def read(text):
        return sql(f'SET search_path TO "{schema}",public;'+text).splitlines()[-1]
    assert read('select count(*) from _sqlx_migrations') == '32'
    assert read('select count(*) from model_requests') == '57746'
    assert read('select count(*) from request_turn_state_observations') == '605149'
    assert read('select count(*) from client_key_charge_events') == '54896'
    assert read("select credential_revision from provider_accounts where id='acct_bridge'") == '2'
    assert read("select revision from codex_quota_policy_accounts where account_id='acct_bridge'") == '4'
    assert read('select sum(amount_usd)::text from client_key_charge_events') == '0.3000000000'
    assert read('select count(*) from admin_users') == '1'
    receipt['old_binary_smoke'] = {'source_commit':'45f72b3a0e366411cd137b6a8def48df7f88439f',
        'artifact_run':37242026140,'artifact_name':'quota-linux-binary',
        'network_internal':True,'external_provider_calls':'network prohibited',
        'healthz_status':health_status,'startup_seconds':round(time.monotonic()-began,3),
        'checks':['old sqlx 1..32 migration checksums accepted','runtime snapshot initialized and HTTP served',
                  'old admin bootstrap writer committed','post-upgrade business rows and revisions readable']}
    receipt_path.write_text(json.dumps(receipt,indent=2),encoding='utf-8')
    print(json.dumps(receipt['old_binary_smoke'],indent=2))
finally:
    subprocess.run(['docker','rm','--force',container],capture_output=True)
    subprocess.run(['docker','network','disconnect',network,args.postgres],capture_output=True)
    subprocess.run(['docker','network','disconnect',network,args.redis],capture_output=True)
    subprocess.run(['docker','network','rm',network],capture_output=True)
