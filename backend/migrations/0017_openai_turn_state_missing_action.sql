-- 缺少可用模型 Turn State 时，账号可选择先自然学习或先执行隔离捕获。
alter table openai_account_turn_state_policies
  add column missing_state_action text not null default 'natural_then_capture'
    check (missing_state_action in ('natural_then_capture', 'capture_first'));
