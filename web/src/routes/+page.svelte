<script lang="ts">
	import { onMount } from 'svelte';
	import { apiFetch, formatIrishTime, type FleetRepoBudget } from '#lib';

	let repos = $state<FleetRepoBudget[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load() {
		try {
			repos = await apiFetch<FleetRepoBudget[]>('/api/v1/budget');
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		load();
	});
</script>

<div class="space-y-4">
	<div class="flex items-center justify-between">
		<h1 class="text-xl font-semibold">Fleet Budget Overview</h1>
		<button class="px-3 py-1.5 text-sm rounded border border-[var(--border)]" onclick={load}>
			Refresh
		</button>
	</div>

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{:else if loading}
		<p class="text-sm text-[var(--fg-muted)]">Loading fleet budget…</p>
	{:else if repos.length === 0}
		<p class="text-sm text-[var(--fg-muted)]">No repositories tracked yet.</p>
	{:else}
		<div class="overflow-x-auto border border-[var(--border)] rounded">
			<table class="w-full text-sm text-left">
				<thead class="border-b border-[var(--border)] text-xs uppercase text-[var(--fg-muted)]">
					<tr>
						<th class="px-4 py-2.5">Repository</th>
						<th class="px-4 py-2.5">Budget</th>
						<th class="px-4 py-2.5">Limit / Stars</th>
						<th class="px-4 py-2.5">Confidence</th>
						<th class="px-4 py-2.5">Queue</th>
						<th class="px-4 py-2.5">Coverage</th>
						<th class="px-4 py-2.5">Findings</th>
					</tr>
				</thead>
				<tbody>
					{#each repos as r (r.repo)}
						<tr class="border-b border-[var(--border)] last:border-b-0">
							<td class="px-4 py-2.5 font-medium">
								<a href="/repo/{r.owner}/{r.name}" class="hover:underline">{r.repo}</a>
							</td>
							<td class="px-4 py-2.5">
								{#if r.free}
									<span class="text-emerald-500 font-medium">Free now ({r.remaining}/{r.limit})</span>
								{:else}
									<span class="text-amber-500">Next {formatIrishTime(r.next_free_at)}</span>
								{/if}
							</td>
							<td class="px-4 py-2.5">{r.limit}/hr · {r.stars}★</td>
							<td class="px-4 py-2.5">{r.confidence}</td>
							<td class="px-4 py-2.5">{r.queue_length}</td>
							<td class="px-4 py-2.5">{r.coverage_pct.toFixed(0)}%</td>
							<td class="px-4 py-2.5">{r.open_findings}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</div>
