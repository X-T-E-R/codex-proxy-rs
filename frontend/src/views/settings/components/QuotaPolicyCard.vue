<script setup lang="ts">
import BaseCard from '@/components/base/BaseCard.vue'
import QuotaPolicyEditor from '@/components/quota-policy/QuotaPolicyEditor.vue'
</script>

<template>
  <BaseCard title="Codex 额度策略" description="OpenAI OAuth 账号的全局默认，账号可继承、禁用或自定义">
    <QuotaPolicyEditor />
  </BaseCard>
</template>
