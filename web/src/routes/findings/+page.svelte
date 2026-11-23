<script lang="ts">
	import { onMount } from 'svelte';
	import { apiFetch, type FindingRow } from '#lib';

	let findings = $state<FindingRow[]>([]);
	let statusFilter = $state('');
	let error = $state<string | null>(null);

	async function load() {
		try {
			const qs = statusFilter ? `?status=${encodeURIComponent(statusFilter)}` : '';
			findings = await apiFetch<FindingRow[]>(`/api/v1/findings${qs}`);
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function setStatus(id: number, status: string) {
		await apiFetch(`/api/v1/findings/${id}`, {
			method: 'PATCH',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ status })
		});
		await load();
	}

	onMount(load);
</script>

<div class="space-y-4">
	<div class="flex items-center justify-between">
		<h1 class="text-xl font-semibold">Findings</h1>
		<select bind:value={statusFilter} onchange={load} class="border border-[var(--border)] rounded px-2 py-1 text-sm bg-transparent">
			<option value="">All statuses</option>
			<option value="new">new</option>
			<option value="real">real</option>
			<option value="false">false</option>
			<option value="fixed">fixed</option>
			<option value="needs-martin">needs-martin</option>
		</select>
	</div>

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{:else if findings.length === 0}
		<p class="text-sm text-[var(--fg-muted)]">No findings recorded.</p>
	{:else}
		<div class="space-y-2">
			{#each findings as f (f.id)}
				<div class="border border-[var(--border)] rounded p-3 flex items-start justify-between gap-4">
					<div>
						<div class="text-xs text-[var(--fg-muted)]">{f.repo} · {f.path}{f.line ? `:${f.line}` : ''} · {f.severity}</div>
						<div class="font-medium text-sm mt-0.5">{f.title}</div>
					</div>
					<div class="flex items-center gap-2 text-xs">
						<span class="px-2 py-0.5 rounded border border-[var(--border)]">{f.status}</span>
						<button class="px-2 py-0.5 rounded border border-[var(--border)]" onclick={() => setStatus(f.id, 'real')}>real</button>
						<button class="px-2 py-0.5 rounded border border-[var(--border)]" onclick={() => setStatus(f.id, 'false')}>false</button>
					</div>
				</div>
			{/each}
		</div>
	{/if}
</div>
