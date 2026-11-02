<script setup lang="ts">
import type { PluginAdminActionDto, PluginAdminComponentDto } from '~/types/api';

const props = defineProps<{
  component: PluginAdminComponentDto;
  data: Record<string, unknown>;
  actions: PluginAdminActionDto[];
}>();

const emit = defineEmits<{
  invoke: [action: string, input: Record<string, unknown>];
}>();

function resolve(path: string): unknown {
  return path.split('.').reduce<unknown>((current, key) => {
    if (!current || typeof current !== 'object') return undefined;
    return (current as Record<string, unknown>)[key];
  }, props.data);
}

function rows(path: string): Array<Record<string, unknown>> {
  const value = resolve(path);
  return Array.isArray(value) ? value.filter((row): row is Record<string, unknown> => !!row && typeof row === 'object') : [];
}

function actionLabel(id: string): string {
  return props.actions.find((action) => action.id === id)?.label ?? id;
}

function actionIsDestructive(id: string): boolean {
  return props.actions.find((action) => action.id === id)?.destructive ?? false;
}
</script>

<template>
  <div v-if="component.kind === 'stack'" class="grid gap-4">
    <PluginAdminRenderer
      v-for="(child, index) in component.children"
      :key="index"
      :component="child"
      :data="data"
      :actions="actions"
      @invoke="(action, input) => emit('invoke', action, input)"
    />
  </div>
  <div
    v-else-if="component.kind === 'grid'"
    class="plugin-admin-grid grid gap-4"
    :style="{ gridTemplateColumns: `repeat(${Math.min(4, Math.max(1, component.columns))}, minmax(0, 1fr))` }"
  >
    <PluginAdminRenderer
      v-for="(child, index) in component.children"
      :key="index"
      :component="child"
      :data="data"
      :actions="actions"
      @invoke="(action, input) => emit('invoke', action, input)"
    />
  </div>
  <section v-else-if="component.kind === 'card'" class="glass-pane min-w-0 p-5">
    <h3 class="mb-4 text-sm font-semibold uppercase tracking-wide text-[var(--ir-text-muted)]">{{ component.title }}</h3>
    <div class="grid gap-4">
      <PluginAdminRenderer
        v-for="(child, index) in component.children"
        :key="index"
        :component="child"
        :data="data"
        :actions="actions"
        @invoke="(action, input) => emit('invoke', action, input)"
      />
    </div>
  </section>
  <StatCard
    v-else-if="component.kind === 'metric'"
    :label="component.label"
    :value="(resolve(component.value_path) as string | number | null | undefined) ?? '—'"
    :hint="component.note ?? undefined"
  />
  <p v-else-if="component.kind === 'text'" class="text-sm text-[var(--ir-text-muted)]">{{ component.content }}</p>
  <div
    v-else-if="component.kind === 'callout'"
    class="rounded-lg border border-[var(--ir-border)] bg-[var(--ir-surface-soft)] p-4 text-sm"
    :class="component.tone === 'bad' ? 'text-red-300' : component.tone === 'good' ? 'text-emerald-300' : component.tone === 'warn' ? 'text-amber-300' : 'text-[var(--ir-text-muted)]'"
  >
    {{ component.content }}
  </div>
  <div v-else-if="component.kind === 'table'" class="grid gap-3">
    <DataTable
      :columns="component.columns"
      :rows="rows(component.data_path)"
      :empty-text="component.empty_text ?? undefined"
      :show-actions="!!component.row_action"
    >
      <template #actions="{ row }">
        <button
          v-if="component.row_action"
          class="btn btn-ghost text-xs"
          :class="actionIsDestructive(component.row_action) ? 'text-[var(--ir-danger)]' : ''"
          type="button"
          @click="emit('invoke', component.row_action, row)"
        >
          {{ actionLabel(component.row_action) }}
        </button>
      </template>
    </DataTable>
  </div>
  <button
    v-else-if="component.kind === 'button'"
    class="btn inline-flex items-center justify-center"
    :class="component.destructive ? 'btn-danger' : 'btn-secondary'"
    type="button"
    @click="emit('invoke', component.action, {})"
  >
    {{ component.label }}
  </button>
</template>

<style scoped>
@media (max-width: 640px) {
  .plugin-admin-grid {
    grid-template-columns: minmax(0, 1fr) !important;
  }
}
</style>
