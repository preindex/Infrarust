<script setup lang="ts">
import { ArrowLeftIcon } from '@heroicons/vue/24/outline';
import type {
  ApiEnvelope,
  PluginAdminActionDto,
  PluginAdminComponentDto,
  PluginAdminFieldDto,
  PluginAdminPageDto,
  PluginDto,
  RestartAwareResult,
} from '~/types/api';

const route = useRoute();
const { request } = useApi();
const { push } = useToast();
const pluginId = route.params.id as string;
const pageSlug = route.params.slug as string;
const plugin = ref<PluginDto | null>(null);
const page = ref<PluginAdminPageDto | null>(null);
const values = ref<Record<string, number | string[]>>({});
const saving = ref(false);
const adminData = ref<Record<string, unknown>>({});
const runningAction = ref<string | null>(null);

const configEndpoint = `/plugins/${encodeURIComponent(pluginId)}/admin/config`;
const { data: initial } = await useAsyncData(`plugin-admin:${pluginId}:${pageSlug}`, async () => {
  try {
    const [pluginResponse, configResponse, dataResponse] = await Promise.all([
      request<ApiEnvelope<PluginDto>>(`/plugins/${encodeURIComponent(pluginId)}`),
      request<ApiEnvelope<{ values: Record<string, number | string[]> }>>(configEndpoint),
      request<ApiEnvelope<{ data: Record<string, unknown> }>>(`/plugins/${encodeURIComponent(pluginId)}/admin/data`),
    ]);
    return { plugin: pluginResponse.data, values: configResponse.data.values, adminData: dataResponse.data.data };
  } catch {
    push({ type: 'error', title: 'Unable to load plugin settings' });
    return null;
  }
});

plugin.value = initial.value?.plugin ?? null;
const contribution = plugin.value?.admin_page;
if (contribution && contribution.slug === pageSlug && contribution.version === 1) {
  page.value = contribution;
  values.value = initial.value?.values ?? {};
  adminData.value = initial.value?.adminData ?? {};
}

function fieldValue(field: PluginAdminFieldDto): number | string[] {
  return values.value[field.key] ?? (field.kind === 'integer' ? (field.default_integer ?? 0) : field.default_strings);
}

function uuidText(field: PluginAdminFieldDto): string {
  const value = fieldValue(field);
  return Array.isArray(value) ? value.join('\n') : '';
}

function setUuidText(field: PluginAdminFieldDto, text: string) {
  values.value[field.key] = text.split(/\r?\n/).map((item) => item.trim()).filter(Boolean);
}

async function invokeAction(actionId: string, input: Record<string, unknown>) {
  if (!page.value || runningAction.value) return;
  const action = page.value.actions.find((item: PluginAdminActionDto) => item.id === actionId);
  if (!action) return;
  if (action.confirmation && !window.confirm(action.confirmation)) return;
  runningAction.value = actionId;
  try {
    await request<ApiEnvelope<unknown>>(
      `/plugins/${encodeURIComponent(pluginId)}/admin/actions/${encodeURIComponent(actionId)}`,
      { method: 'POST', body: { input } },
    );
    const refreshed = await request<ApiEnvelope<{ data: Record<string, unknown> }>>(
      `/plugins/${encodeURIComponent(pluginId)}/admin/data`,
    );
    adminData.value = refreshed.data.data;
    push({ type: 'success', title: `${action.label} completed` });
  } catch {
    push({ type: 'error', title: `Failed to ${action.label.toLowerCase()}` });
  } finally {
    runningAction.value = null;
  }
}

async function save() {
  if (!page.value) return;
  saving.value = true;
  try {
    const result = await request<ApiEnvelope<RestartAwareResult>>(configEndpoint, {
      method: 'PUT',
      body: { values: values.value },
    });
    push({
      type: 'success',
      title: 'Settings saved',
      message: result.data.message,
    });
  } catch {
    push({ type: 'error', title: 'Failed to save plugin settings' });
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="grid gap-5">
    <NuxtLink
      :to="`/plugins/${encodeURIComponent(pluginId)}`"
      class="inline-flex items-center gap-1.5 text-sm text-[var(--ir-text-muted)] transition-colors hover:text-white"
    >
      <ArrowLeftIcon class="h-4 w-4" />
      Back to {{ plugin?.name ?? 'plugin' }}
    </NuxtLink>

    <section v-if="page" class="glass-pane p-5 sm:p-6">
      <div class="mb-6">
        <h2 class="text-xl font-bold tracking-tight">{{ page.title }}</h2>
        <p v-if="page.description" class="mt-2 max-w-3xl text-sm text-[var(--ir-text-muted)]">
          {{ page.description }}
        </p>
      </div>

      <div v-if="page.components.length" class="mb-6 grid gap-4">
        <PluginAdminRenderer
          v-for="(component, index) in page.components"
          :key="index"
          :component="component as PluginAdminComponentDto"
          :data="adminData"
          :actions="page.actions"
          @invoke="invokeAction"
        />
      </div>

      <div v-if="page.fields.length" class="grid gap-5">
        <label v-for="field in page.fields" :key="field.key" class="grid gap-2">
          <span class="text-sm font-medium">{{ field.label }}</span>
          <span v-if="field.description" class="text-xs text-[var(--ir-text-muted)]">
            {{ field.description }}
          </span>
          <input
            v-if="field.kind === 'integer'"
            :value="fieldValue(field)"
            type="number"
            inputmode="numeric"
            :min="field.minimum ?? undefined"
            :max="field.maximum ?? undefined"
            step="1"
            class="w-full rounded-lg border border-[var(--ir-border)] bg-[var(--ir-surface-soft)] px-3 py-2 text-sm text-white outline-none focus:border-[var(--ir-accent)]"
            @input="values[field.key] = Number(($event.target as HTMLInputElement).value)"
          />
          <textarea
            v-else
            :value="uuidText(field)"
            rows="5"
            spellcheck="false"
            placeholder="One UUID per line"
            class="w-full resize-y rounded-lg border border-[var(--ir-border)] bg-[var(--ir-surface-soft)] px-3 py-2 font-mono text-sm text-white outline-none focus:border-[var(--ir-accent)]"
            @input="setUuidText(field, ($event.target as HTMLTextAreaElement).value)"
          />
          <span v-if="field.kind === 'uuid_list'" class="text-xs text-[var(--ir-text-muted)]">
            Up to {{ field.max_items }} UUIDs, one per line.
          </span>
        </label>
      </div>

      <div v-if="page.fields.length" class="mt-6 flex flex-col gap-3 border-t border-[var(--ir-border)] pt-4 sm:flex-row sm:items-center sm:justify-between">
        <p class="text-xs text-[var(--ir-text-muted)]">Saved changes require a proxy restart.</p>
        <button
          type="button"
          :disabled="saving"
          class="rounded-lg bg-[var(--ir-accent)] px-4 py-2 text-sm font-semibold text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
          @click="save"
        >
          {{ saving ? 'Saving…' : 'Save settings' }}
        </button>
      </div>
    </section>

    <div v-else class="glass-pane p-5 text-sm text-[var(--ir-text-muted)]">
      This plugin page is unavailable or uses an unsupported schema.
    </div>
  </div>
</template>
