<script lang="ts">
	import { onMount } from 'svelte';
	import { apiFetch, type Settings } from '#lib';

	let settings = $state<Settings | null>(null);
	let error = $state<string | null>(null);

	async function load() {
		try {
			settings = await apiFetch<Settings>('/api/v1/settings');
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function updateMode(mode: string) {
		settings = await apiFetch<Settings>('/api/v1/settings', {
			method: 'PATCH',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ mode })
		});
	}

	onMount(load);
</script>

<div class="space-y-4">
	<h1 class="text-xl font-semibold">Settings</h1>

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{:else if !settings}
		<p class="text-sm text-[var(--fg-muted)]">Loading settings…</p>
	{:else}
		<div class="border border-[var(--border)] rounded p-4 space-y-3 text-sm">
			<div class="flex items-center justify-between">
				<span>Mode</span>
				<div class="flex gap-2">
					{#each ['observe', 'on-demand', 'full'] as m}
						<button
							class="px-3 py-1 rounded border border-[var(--border)]"
							class:font-semibold={settings.mode === m}
							onclick={() => updateMode(m)}
						>
							{m}
						</button>
					{/each}
				</div>
			</div>
			<div class="flex items-center justify-between">
				<span>Repo daily proactive cap</span>
				<span>{settings.repo_daily_cap}</span>
			</div>
			<div class="flex items-center justify-between">
				<span>Fleet daily proactive cap</span>
				<span>{settings.fleet_daily_cap}</span>
			</div>
			<div class="flex items-center justify-between">
				<span>Quiet minutes</span>
				<span>{settings.quiet_minutes}</span>
			</div>
		</div>
	{/if}
</div>
