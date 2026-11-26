<script lang="ts">
	import { onMount } from 'svelte';
	import { apiFetch, type Settings } from '#lib';

	let settings = $state<Settings | null>(null);
	let error = $state<string | null>(null);
	let statusMsg = $state<string | null>(null);
	let saving = $state(false);
	let repoCap = $state('6');
	let fleetCap = $state('40');
	let quietMinutes = $state('45');

	function syncInputs(s: Settings) {
		repoCap = String(s.repo_daily_cap);
		fleetCap = String(s.fleet_daily_cap);
		quietMinutes = String(s.quiet_minutes);
	}

	async function load() {
		try {
			const s = await apiFetch<Settings>('/api/v1/settings');
			settings = s;
			syncInputs(s);
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function updateMode(mode: string) {
		if (saving) return;
		saving = true;
		statusMsg = null;
		try {
			const s = await apiFetch<Settings>('/api/v1/settings', {
				method: 'PATCH',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ mode })
			});
			settings = s;
			syncInputs(s);
			statusMsg = `Mode set to ${s.mode}.`;
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			saving = false;
		}
	}

	async function saveCaps() {
		if (saving) return;
		const rCap = Number(repoCap.trim());
		const fCap = Number(fleetCap.trim());
		const qMin = Number(quietMinutes.trim());
		if (
			!Number.isSafeInteger(rCap) || rCap < 0 || rCap > 500 ||
			!Number.isSafeInteger(fCap) || fCap < 0 || fCap > 500 ||
			!Number.isSafeInteger(qMin) || qMin < 0 || qMin > 1440
		) {
			error = 'Caps must be integers 0–500 and quiet minutes 0–1440.';
			return;
		}
		saving = true;
		statusMsg = null;
		try {
			const s = await apiFetch<Settings>('/api/v1/settings', {
				method: 'PATCH',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({
					repo_daily_cap: rCap,
					fleet_daily_cap: fCap,
					quiet_minutes: qMin
				})
			});
			settings = s;
			syncInputs(s);
			statusMsg = 'Settings saved.';
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			saving = false;
		}
	}

	onMount(load);
</script>

<div class="space-y-4">
	<h1 class="text-xl font-semibold">Settings</h1>

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{/if}
	{#if statusMsg}
		<p class="text-sm text-emerald-500">{statusMsg}</p>
	{/if}

	{#if !settings}
		<p class="text-sm text-[var(--fg-muted)]">Loading settings…</p>
	{:else}
		<div class="border border-[var(--border)] rounded p-4 space-y-3 text-sm">
			<div class="flex items-center justify-between">
				<span>Mode</span>
				<div class="flex gap-2">
					{#each ['observe', 'on-demand', 'full'] as m}
						<button
							class="px-3 py-1 rounded border border-[var(--border)] disabled:opacity-50"
							class:font-semibold={settings.mode === m}
							disabled={saving}
							onclick={() => updateMode(m)}
						>
							{m}
						</button>
					{/each}
				</div>
			</div>
			<div class="flex items-center justify-between">
				<label for="repo-cap">Repo daily proactive cap</label>
				<input
					id="repo-cap"
					type="number"
					min="0"
					max="500"
					step="1"
					bind:value={repoCap}
					class="w-24 px-2 py-1 rounded border border-[var(--border)] bg-transparent text-right"
				/>
			</div>
			<div class="flex items-center justify-between">
				<label for="fleet-cap">Fleet daily proactive cap</label>
				<input
					id="fleet-cap"
					type="number"
					min="0"
					max="500"
					step="1"
					bind:value={fleetCap}
					class="w-24 px-2 py-1 rounded border border-[var(--border)] bg-transparent text-right"
				/>
			</div>
			<div class="flex items-center justify-between">
				<label for="quiet-mins">Quiet minutes (background proactive)</label>
				<input
					id="quiet-mins"
					type="number"
					min="0"
					max="1440"
					step="1"
					bind:value={quietMinutes}
					class="w-24 px-2 py-1 rounded border border-[var(--border)] bg-transparent text-right"
				/>
			</div>
			<div class="flex justify-end pt-1">
				<button
					class="px-3 py-1 rounded border border-[var(--border)] hover:bg-[var(--bg-subtle)] disabled:opacity-50"
					disabled={saving}
					onclick={saveCaps}
				>
					Save
				</button>
			</div>
		</div>
	{/if}
</div>
