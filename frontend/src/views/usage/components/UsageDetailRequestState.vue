<script setup lang="ts">
import type { UsageListRecord } from '@/api'
import { ZButton, ZEmpty, ZSkeleton } from '@codex-proxy/ui'

import { TriangleAlert } from '@lucide/vue'
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'

const props = defineProps<{
  loading: boolean
  error: string
  target: UsageListRecord | null
}>()

const emit = defineEmits<{ retry: [] }>()

const { t } = useI18n({ useScope: 'global' })

// 加载与失败都保留目标请求的时间、模型，避免连续点开时认错记录
const targetText = computed(() => props.target
  ? t('usage.request.detailTarget', {
      time: props.target.createdAtDisplay,
      model: props.target.model ?? t('common.values.unknown'),
    })
  : '')
</script>

<template>
  <div v-if="loading" class="grid min-w-0 gap-3" role="status" aria-busy="true">
    <span class="sr-only">{{ t('usage.request.loadingDetails') }}</span>
    <p v-if="targetText" class="m-0 min-w-0 truncate text-cp-xs font-emphasis text-cp-text-secondary">
      {{ targetText }}
    </p>
    <div class="grid min-w-0 gap-3 lg:grid-cols-2">
      <section
        v-for="panel in 2"
        :key="panel"
        class="min-w-0 rounded-cp-card bg-cp-fill-quaternary px-4 py-3.5"
      >
        <ZSkeleton shape="text" class="w-24" />
        <div class="mt-4 grid grid-cols-2 gap-x-4 gap-y-3">
          <div v-for="field in 4" :key="field" class="grid min-w-0 gap-2">
            <ZSkeleton shape="text" class="w-14 opacity-70" />
            <ZSkeleton shape="text" class="w-full max-w-28" />
          </div>
        </div>
      </section>
    </div>
    <section class="min-w-0 rounded-cp-card bg-cp-fill-quaternary px-4 py-3.5">
      <ZSkeleton shape="text" class="w-20" />
      <div class="mt-4 grid gap-3 sm:grid-cols-3">
        <ZSkeleton v-for="field in 3" :key="field" shape="text" class="w-full max-w-40" />
      </div>
    </section>
  </div>

  <div v-else class="grid min-w-0 gap-3">
    <p v-if="targetText" class="m-0 min-w-0 truncate text-cp-xs font-emphasis text-cp-text-secondary">
      {{ targetText }}
    </p>
    <ZEmpty
      :title="t('usage.request.detailFailed')"
      :description="error"
      :icon="TriangleAlert"
      surface="none"
      role="alert"
    >
      <template #action>
        <ZButton :loading="loading" @click="emit('retry')">
          {{ t('common.actions.retry') }}
        </ZButton>
      </template>
    </ZEmpty>
  </div>
</template>
