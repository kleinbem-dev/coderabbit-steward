<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { apiFetch, formatIrishTime, type RepoDetail } from '#lib';

	let detail = $state<RepoDetail | null>(null);
	let error = $state<string | null>(null);

	const owner = $derived(page.params.owner);
	const name = $derived(page.params.name);

	onMount(async () => {
		try {
			detail = await apiFetch<RepoDetail>(`/api/v1/repo/${owner}/${name}`);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	});
</script>

<div class="space-y-6">
	<div>
		<a href="/" class="text-sm text-[var(--fg-muted)] hover:underline">← Fleet</a>
		<h1 class="text-xl font-semibold mt-1">{owner}/{name}</h1>
	</div>

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{:else if !detail}
		<p class="text-sm text-[var(--fg-muted)]">Loading repository…</p>
	{:else}
		<section class="grid grid-cols-1 md:grid-cols-3 gap-4">
			<div class="border border-[var(--border)] rounded p-4">
				<div class="text-xs text-[var(--fg-muted)]">Budget</div>
				<div class="text-lg font-semibold mt-1">
					{detail.budget.free ? 'Free now' : `Next ${formatIrishTime(detail.budget.next_free_at)}`}
				</div>
				<div class="text-xs text-[var(--fg-muted)] mt-1">
					{detail.budget.remaining}/{detail.budget.limit} · refill {detail.budget.refill_minutes}m · {detail.budget.confidence}
				</div>
			</div>
			<div class="border border-[var(--border)] rounded p-4">
				<div class="text-xs text-[var(--fg-muted)]">Queue</div>
				<div class="text-lg font-semibold mt-1">{detail.queue.length} entries</div>
			</div>
			<div class="border border-[var(--border)] rounded p-4">
				<div class="text-xs text-[var(--fg-muted)]">Findings</div>
				<div class="text-lg font-semibold mt-1">{detail.findings.length} recorded</div>
			</div>
		</section>

		<section class="space-y-2">
			<h2 class="text-base font-semibold">Recent Spends</h2>
			{#if detail.spends.length === 0}
				<p class="text-sm text-[var(--fg-muted)]">No spends recorded.</p>
			{:else}
				<div class="border border-[var(--border)] rounded overflow-x-auto">
					<table class="w-full text-sm text-left">
						<thead class="border-b border-[var(--border)] text-xs uppercase text-[var(--fg-muted)]">
							<tr>
								<th class="px-4 py-2">Time</th>
								<th class="px-4 py-2">Identity</th>
								<th class="px-4 py-2">Cause</th>
								<th class="px-4 py-2">PR</th>
								<th class="px-4 py-2">State</th>
							</tr>
						</thead>
						<tbody>
							{#each detail.spends as s (s.id)}
								<tr class="border-b border-[var(--border)] last:border-b-0">
									<td class="px-4 py-2">{formatIrishTime(s.at)}</td>
									<td class="px-4 py-2">{s.identity}</td>
									<td class="px-4 py-2">{s.cause}</td>
									<td class="px-4 py-2">{s.pr ? `#${s.pr}` : '—'}</td>
									<td class="px-4 py-2">{s.state}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
	{/if}
</div>
