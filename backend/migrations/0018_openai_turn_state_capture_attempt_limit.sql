-- 扩大可配置尝试次数；保留已有账号配置、默认值和任务总超时。
alter table openai_account_turn_state_policies
  drop constraint openai_account_turn_state_policies_max_attempts_check,
  add constraint openai_account_turn_state_policies_max_attempts_check
    check (max_attempts between 1 and 50);
