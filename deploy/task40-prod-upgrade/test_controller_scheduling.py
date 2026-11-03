"""本地阶段授权、延迟调度与真实原子收据发布竞争，不连接外部服务"""
from contextlib import contextmanager
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

from test_schema53 import accepted, controller, remote, schema53


class ControllerSchedulingTest(unittest.TestCase):
    def test_queued_authorization_cannot_be_replaced_before_supervisor_gets_lock(self):
        for operation, following in (('activate', 'cleanup'), ('cleanup', 'activate')):
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as tmp:
                run = Path(tmp)
                remote.atomic(run / 'controller.json', {
                    'accepted': accepted(), 'remoteRoot': '/unused', 'operation': 'prepare',
                    'phase': 'finished', 'schemaMayHaveStarted': True,
                    'prepareReceipt': {'phase': 'schema53_prepared'},
                })
                with patch.object(controller, 'action_lock', return_value=MagicMock()), \
                     patch.object(controller, 'reconcile_preparation', return_value={'phase': 'schema53_prepared'}), \
                     patch.object(controller, 'spawn_supervisor', return_value={}) as spawn:
                    controller.continue_schema53(run, operation)
                    original = (run / 'controller.json').read_bytes()
                    with self.assertRaisesRegex(RuntimeError, 'previous_operation_unsettled'):
                        controller.continue_schema53(run, following)
                spawn.assert_called_once_with(run, operation)
                self.assertEqual((run / 'controller.json').read_bytes(), original)
                with patch.object(controller, 'action_lock', return_value=MagicMock()), \
                     patch.object(controller, 'start_remote', return_value={}) as activate, \
                     patch.object(controller, 'start_remote_preparation', return_value={}) as cleanup, \
                     patch.object(controller, 'verify_remote', return_value={'phase': 'verified'}), \
                     patch.object(controller, 'observe', return_value={
                         'phase': 'deployed' if operation == 'activate' else 'schema53_cleaned'}):
                    controller.work(run, operation)
                    finished = (run / 'controller.json').read_bytes()
                    with self.assertRaisesRegex(RuntimeError, 'authorized_operation_binding_required'):
                        controller.work(run, following)
                    with self.assertRaisesRegex(RuntimeError, 'authorized_operation_binding_required'):
                        controller.supervise(run, following)
                self.assertEqual((run / 'controller.json').read_bytes(), finished)
                self.assertEqual((activate.call_count, cleanup.call_count),
                                 (1, 0) if operation == 'activate' else (0, 1))
                self.assertEqual(controller.read(run / 'controller.json')['pendingOperation'], operation)

    def test_failed_prepare_then_cleanup_keeps_each_authorization_and_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            failed = {'phase': 'schema53_preparation_requires_operator'}
            cleaned = {'phase': 'schema53_cleaned'}
            remote.atomic(run / 'controller.json', {
                'accepted': accepted(), 'remoteRoot': '/unused', 'operation': 'prepare',
                'pendingOperation': 'prepare', 'phase': 'prepared', 'schema53Artifacts': str(schema53.BUNDLE),
            })
            def child(argv, **kwargs):
                operation = argv[argv.index('--operation') + 1]
                return SimpleNamespace(returncode=0, wait=lambda: controller.work(run, operation))
            with patch.object(controller, 'action_lock', return_value=MagicMock()), \
                 patch.object(controller, 'provision') as provision, patch.object(controller, 'pull_exact') as pull, \
                 patch.object(controller, 'start_remote_preparation', return_value={}) as start, \
                 patch.object(controller, 'observe', side_effect=[failed, cleaned]), \
                 patch.object(controller, 'spawn_supervisor', return_value={}) as spawn, \
                 patch.object(controller.subprocess, 'Popen', side_effect=child):
                controller.supervise(run, 'prepare')
                self.assertNotIn('pendingOperation', controller.read(run / 'controller.json'))
                controller.continue_schema53(run, 'cleanup')
                controller.supervise(run, 'cleanup')
            spawn.assert_called_once_with(run, 'cleanup')
            self.assertEqual([call.kwargs['cleanup'] for call in start.call_args_list], [False, True])
            self.assertEqual((provision.call_count, pull.call_count), (1, 1))
            value = controller.read(run / 'controller.json')
            self.assertEqual((value['prepareReceipt'], value['cleanupReceipt']), (failed, cleaned))
            self.assertNotIn('pendingOperation', value)
            self.assertEqual(controller.read(run / 'supervisor.json')['operation'], 'cleanup')
            self.assertTrue((run / 'supervisor-prepare.json').exists())
            self.assertTrue((run / 'supervisor-cleanup.json').exists())

    def test_receipt_fsync_blocks_next_stage_until_stage_and_summary_are_published(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            remote.atomic(run / 'controller.json', {
                'accepted': accepted(), 'remoteRoot': '/unused', 'operation': 'prepare',
                'pendingOperation': 'prepare', 'phase': 'finished', 'schemaMayHaveStarted': True,
            })
            mutex, publishing = threading.Lock(), threading.local()
            ready, release, second_waiting, authorized = (threading.Event() for _ in range(4))
            original_save, original_fsync = controller.save, remote.os.fsync
            errors, worker_operations = [], []
            @contextmanager
            def lock(path, **kwargs):
                if threading.current_thread().name == 'activate-supervisor' and path.name == 'controller-active.lock':
                    second_waiting.set()
                with mutex:
                    yield
            def save(path, value):
                publishing.name = path.name
                try:
                    original_save(path, value)
                finally:
                    publishing.name = None
            def fsync(fd):
                if (threading.current_thread().name == 'prepare-supervisor' and
                        getattr(publishing, 'name', None) == 'supervisor.json' and not ready.is_set()):
                    ready.set()
                    if not release.wait(10):
                        raise RuntimeError('bounded_schedule_timeout')
                return original_fsync(fd)
            def child(argv, **kwargs):
                worker_operations.append(argv[argv.index('--operation') + 1])
                return SimpleNamespace(returncode=0, wait=lambda: None)
            def first():
                try:
                    controller.supervise(run, 'prepare')
                except BaseException as error:
                    errors.append(error)
            def second():
                try:
                    controller.continue_schema53(run, 'activate')
                    authorized.set()
                    # 模拟已绑定 worker 正常退出，发布仍走原 supervisor 与真实 atomic
                    value = controller.read(run / 'controller.json')
                    value['phase'] = 'finished'
                    remote.atomic(run / 'controller.json', value)
                    controller.supervise(run, 'activate')
                except BaseException as error:
                    errors.append(error)
            # 第二阶段授权同时需要worker锁；本调度用可重入锁复现相同排他边界
            mutex = threading.RLock()
            with patch.object(controller, 'action_lock', side_effect=lock), \
                 patch.object(controller, 'save', side_effect=save), \
                 patch.object(controller, 'reconcile_preparation', return_value={'phase': 'schema53_prepared'}), \
                 patch.object(controller, 'spawn_supervisor', return_value={}), \
                 patch.object(controller.subprocess, 'Popen', side_effect=child), \
                 patch.object(remote.os, 'fsync', side_effect=fsync):
                first_thread = threading.Thread(target=first, name='prepare-supervisor')
                second_thread = threading.Thread(target=second, name='activate-supervisor')
                first_thread.start()
                try:
                    self.assertTrue(ready.wait(10), 'prepare did not reach actual summary atomic fsync')
                    self.assertTrue((run / 'supervisor.json.tmp').exists())
                    second_thread.start()
                    self.assertTrue(second_waiting.wait(10))
                    self.assertFalse(authorized.is_set())
                    self.assertEqual(controller.read(run / 'controller.json')['operation'], 'prepare')
                finally:
                    release.set()
                    first_thread.join(10)
                    if second_thread.ident is not None:
                        second_thread.join(10)
            self.assertFalse(first_thread.is_alive() or second_thread.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(worker_operations, ['prepare', 'activate'])
            for operation in ('prepare', 'activate'):
                value = controller.read(run / ('supervisor-' + operation + '.json'))
                self.assertEqual((value['operation'], value['workerExit']), (operation, 0))
                self.assertEqual(value['localLocks'], {'controller': 'held_during_publication', 'worker': 'process_exited'})
            latest = controller.read(run / 'supervisor.json')
            self.assertEqual(latest['operation'], 'activate')
            original = (run / 'supervisor.json').read_bytes()
            with patch.object(controller, 'action_lock', return_value=MagicMock()):
                with self.assertRaisesRegex(RuntimeError, 'authorized_operation_binding_required'):
                    controller.supervise(run, 'prepare')
            self.assertEqual((run / 'supervisor.json').read_bytes(), original)
            self.assertFalse(list(run.glob('*.tmp')))
            self.assertNotIn('pendingOperation', controller.read(run / 'controller.json'))

    def test_detached_spawn_carries_the_authorized_operation(self):
        with patch.object(controller.subprocess, 'Popen') as child, \
             patch.object(controller.subprocess, 'DETACHED_PROCESS', 8, create=True), \
             patch.object(controller.subprocess, 'CREATE_NEW_PROCESS_GROUP', 512, create=True):
            result = controller.spawn_supervisor(Path('/unused'), 'activate')
        self.assertEqual(child.call_args.args[0][-2:], ['--operation', 'activate'])
        self.assertEqual(result['operation'], 'activate')


if __name__ == '__main__':
    unittest.main()
