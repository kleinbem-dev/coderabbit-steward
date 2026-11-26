<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { apiFetch, formatIrishTime, type RepoDetail } from '#lib';

	let detail = $state<RepoDetail | null>(null);
	let error = $state<string | null>(null);
	let prInput = $state('');
	let prBusy = $state(false);
	let fullBusy = $state(false);
	let proactiveBusy = $state(false);
	let queueBusy = $state(false);
	let actionMsg = $state<string | null>(null);

	const owner = $derived(page.params.owner);
	const name = $derived(page.params.name);

	async function load() {
		try {
			detail = await apiFetch<RepoDetail>(`/api/v1/repo/${owner}/${name}`);
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	async function queuePr() {
		if (prBusy) return;
		const pr = Number(prInput.trim());
		if (!Number.isSafeInteger(pr) || pr <= 0) return;
		prBusy = true;
		actionMsg = null;
		try {
			await apiFetch('/api/v1/reviews', {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ repo: `${owner}/${name}`, pr })
			});
			prInput = '';
			actionMsg = `Queued PR #${pr} for review (costs 1 slot when free).`;
			await load();
		} catch (e) {
			actionMsg = e instanceof Error ? e.message : String(e);
		} finally {
			prBusy = false;
		}
	}

	async function queueFullRepo() {
		if (fullBusy) return;
		fullBusy = true;
		actionMsg = null;
		try {
			await apiFetch('/api/v1/reviews', {
				method: 'POST',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ repo: `${owner}/${name}`, full: true })
			});
			actionMsg = 'Queued full-repo review batches (spread over hours under daily caps).';
			await load();
		} catch (e) {
			actionMsg = e instanceof Error ? e.message : String(e);
		} finally {
			fullBusy = false;
		}
	}

	async function toggleProactive() {
		if (proactiveBusy || !detail) return;
		const next = !detail.proactive_enabled;
		proactiveBusy = true;
		actionMsg = null;
		try {
			await apiFetch(`/api/v1/repo/${owner}/${name}`, {
				method: 'PATCH',
				headers: { 'Content-Type': 'application/json' },
				body: JSON.stringify({ proactive_enabled: next })
			});
			actionMsg = next
				? 'Automatic background proactive reviews enabled for this repo.'
				: 'Automatic background proactive reviews paused (manual Full Repo Review still works).';
			await load();
		} catch (e) {
			actionMsg = e instanceof Error ? e.message : String(e);
		} finally {
			proactiveBusy = false;
		}
	}

	async function deleteQueueItem(id: number) {
		if (queueBusy) return;
		queueBusy = true;
		actionMsg = null;
		try {
			await apiFetch(`/api/v1/queue/${id}`, { method: 'DELETE' });
			actionMsg = `Removed queue entry #${id}.`;
			await load();
		} catch (e) {
			actionMsg = e instanceof Error ? e.message : String(e);
		} finally {
			queueBusy = false;
		}
	}

	async function clearWaitingQueue() {
		if (queueBusy) return;
		queueBusy = true;
		actionMsg = null;
		try {
			const res = await apiFetch<{ deleted: number }>(`/api/v1/repo/${owner}/${name}/queue`, {
				method: 'DELETE'
			});
			actionMsg = `Cleared ${res.deleted} waiting queue entry(s).`;
			await load();
		} catch (e) {
			actionMsg = e instanceof Error ? e.message : String(e);
		} finally {
			queueBusy = false;
		}
	}

	onMount(load);
</script>

<div class="space-y-6">
	<div class="flex flex-wrap items-end justify-between gap-4">
		<div>
			<a href="/" class="text-sm text-[var(--fg-muted)] hover:underline">← Fleet</a>
			<h1 class="text-xl font-semibold mt-1">{owner}/{name}</h1>
		</div>
		<div class="flex flex-wrap items-center gap-2 text-sm">
			<input
				type="number"
				min="1"
				step="1"
				placeholder="PR #"
				bind:value={prInput}
				class="w-24 px-2 py-1 rounded border border-[var(--border)] bg-transparent"
			/>
			<button
				class="px-3 py-1 rounded border border-[var(--border)] hover:bg-[var(--bg-subtle)] disabled:opacity-50"
				disabled={prBusy}
				onclick={queuePr}
			>
				Queue PR
			</button>
			<button
				class="px-3 py-1 rounded border border-[var(--border)] hover:bg-[var(--bg-subtle)] disabled:opacity-50"
				disabled={fullBusy}
				onclick={queueFullRepo}
			>
				Full Repo Review
			</button>
			{#if detail}
				<button
					class="px-3 py-1 rounded border border-[var(--border)] hover:bg-[var(--bg-subtle)] disabled:opacity-50"
					disabled={proactiveBusy}
					onclick={toggleProactive}
				>
					Proactive: {detail.proactive_enabled ? 'On' : 'Paused'}
				</button>
			{/if}
		</div>
	</div>

	{#if actionMsg}
		<p class="text-sm text-emerald-500">{actionMsg}</p>
	{/if}

	{#if error}
		<p class="text-sm text-red-500">{error}</p>
	{:else if !detail}
		<p class="text-sm text-[var(--fg-muted)]">Loading repository…</p>
	{:else}
		<section class="grid grid-cols-1 md:grid-cols-4 gap-4">
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
				<div class="text-xs text-[var(--fg-muted)]">Coverage</div>
				<div class="text-lg font-semibold mt-1">{detail.coverage.percentage.toFixed(1)}%</div>
				<div class="text-xs text-[var(--fg-muted)] mt-1">
					{detail.coverage.reviewed_files}/{detail.coverage.total_files} files reviewed
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
			<div class="flex items-center justify-between">
				<h2 class="text-base font-semibold">Queue ({detail.queue.length})</h2>
				{#if detail.queue.some((q) => q.state === 'waiting')}
					<button
						class="px-2.5 py-1 text-xs rounded border border-[var(--border)] hover:bg-[var(--bg-subtle)] disabled:opacity-50"
						disabled={queueBusy}
						onclick={clearWaitingQueue}
					>
						Clear Waiting
					</button>
				{/if}
			</div>
			{#if detail.queue.length === 0}
				<p class="text-sm text-[var(--fg-muted)]">No queued reviews.</p>
			{:else}
				<div class="border border-[var(--border)] rounded overflow-x-auto">
					<table class="w-full text-sm text-left">
						<thead class="border-b border-[var(--border)] text-xs uppercase text-[var(--fg-muted)]">
							<tr>
								<th class="px-4 py-2">ID</th>
								<th class="px-4 py-2">Kind</th>
								<th class="px-4 py-2">PR</th>
								<th class="px-4 py-2">State</th>
								<th class="px-4 py-2">Queued</th>
								<th class="px-4 py-2 text-right">Action</th>
							</tr>
						</thead>
						<tbody>
							{#each detail.queue as q (q.id)}
								<tr class="border-b border-[var(--border)] last:border-b-0">
									<td class="px-4 py-2 font-mono text-xs">#{q.id}</td>
									<td class="px-4 py-2">{q.kind}</td>
									<td class="px-4 py-2">{q.pr_number ? `#${q.pr_number}` : '—'}</td>
									<td class="px-4 py-2">{q.state}</td>
									<td class="px-4 py-2">{formatIrishTime(q.created_at)}</td>
									<td class="px-4 py-2 text-right">
										<button
											class="px-2 py-0.5 text-xs rounded border border-[var(--border)] text-red-500 hover:bg-[var(--bg-subtle)] disabled:opacity-50"
											disabled={queueBusy}
											onclick={() => deleteQueueItem(q.id)}
										>
											Delete
										</button>
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>

		<section class="space-y-2">
			<h2 class="text-base font-semibold">
				Unreviewed Files ({detail.coverage.unreviewed_files.length})
			</h2>
			{#if detail.coverage.unreviewed_files.length === 0}
				<p class="text-sm text-[var(--fg-muted)]">All tracked files are reviewed (or initial scan pending).</p>
			{:else}
				<div class="border border-[var(--border)] rounded overflow-x-auto">
					<table class="w-full text-sm text-left">
						<thead class="border-b border-[var(--border)] text-xs uppercase text-[var(--fg-muted)]">
							<tr>
								<th class="px-4 py-2">Path</th>
								<th class="px-4 py-2">Lines</th>
								<th class="px-4 py-2">Since</th>
								<th class="px-4 py-2">Priority</th>
								<th class="px-4 py-2">Last Reviewed</th>
							</tr>
						</thead>
						<tbody>
							{#each detail.coverage.unreviewed_files.slice(0, 100) as f (f.path)}
								<tr class="border-b border-[var(--border)] last:border-b-0">
									<td class="px-4 py-2 font-mono text-xs">
										{f.path}
										{#if f.risky}
											<span class="ml-1.5 px-1.5 py-0.5 rounded bg-amber-500/20 text-amber-500 text-[10px] uppercase">risky</span>
										{/if}
									</td>
									<td class="px-4 py-2">{f.unreviewed_lines}</td>
									<td class="px-4 py-2">{formatIrishTime(f.unreviewed_since)}</td>
									<td class="px-4 py-2">{f.priority.toFixed(1)}</td>
									<td class="px-4 py-2 font-mono text-xs">
										{f.reviewed_commit ? f.reviewed_commit.slice(0, 7) : 'never'}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
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
