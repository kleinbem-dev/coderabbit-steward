<script lang="ts">
	import '../app.css';
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';

	let { children }: { children?: Snippet } = $props();

	function readStoredToken(): string {
		try {
			return typeof localStorage !== 'undefined' ? (localStorage.getItem('cr_steward_token') ?? '') : '';
		} catch {
			return '';
		}
	}

	let showToken = $state(false);
	let tokenInput = $state(readStoredToken());
	let tokenError = $state<string | null>(null);

	function saveToken(e: SubmitEvent) {
		e.preventDefault();
		tokenError = null;
		const trimmed = tokenInput.trim();
		try {
			if (trimmed) {
				localStorage.setItem('cr_steward_token', trimmed);
			} else {
				localStorage.removeItem('cr_steward_token');
			}
			tokenInput = trimmed;
			showToken = false;
			location.reload();
		} catch (err) {
			tokenError = err instanceof Error ? err.message : 'Unable to save token to browser storage';
		}
	}

	function clearToken() {
		tokenError = null;
		try {
			localStorage.removeItem('cr_steward_token');
			tokenInput = '';
			showToken = false;
			location.reload();
		} catch (err) {
			tokenError = err instanceof Error ? err.message : 'Unable to clear token from browser storage';
		}
	}
</script>

<div class="min-h-screen bg-[var(--bg)] text-[var(--fg)]">
	<header class="border-b border-[var(--border)] px-6 py-3 flex items-center justify-between">
		<div class="flex items-center gap-6">
			<a href="/" class="font-semibold tracking-tight text-base">coderabbit-steward</a>
			<nav class="flex items-center gap-4 text-sm text-[var(--fg-muted)]">
				<a href="/" class:font-medium={page.url.pathname === '/'}>Fleet</a>
				<a href="/findings" class:font-medium={page.url.pathname === '/findings'}>Findings</a>
				<a href="/settings" class:font-medium={page.url.pathname === '/settings'}>Settings</a>
			</nav>
		</div>
		<div class="flex items-center gap-3 text-xs text-[var(--fg-muted)]">
			<button
				type="button"
				class="px-2 py-1 rounded border border-[var(--border)] hover:text-[var(--fg)]"
				onclick={() => (showToken = !showToken)}
			>
				{tokenInput.trim() ? 'API Token ✓' : 'API Token'}
			</button>
			<span>Europe/Dublin</span>
		</div>
	</header>

	{#if showToken}
		<div class="border-b border-[var(--border)] px-6 py-3">
			<form class="max-w-6xl mx-auto flex flex-wrap items-center gap-2 text-sm" onsubmit={saveToken}>
				<input
					type="password"
					bind:value={tokenInput}
					placeholder="CR_STEWARD_API_TOKEN"
					aria-label="API Bearer Token"
					autocomplete="off"
					class="flex-1 min-w-64 px-2.5 py-1 rounded border border-[var(--border)] bg-transparent"
				/>
				<button type="submit" class="px-3 py-1 rounded border border-[var(--border)]">Save</button>
				{#if tokenInput.trim()}
					<button type="button" class="px-3 py-1 rounded border border-[var(--border)]" onclick={clearToken}>
						Clear
					</button>
				{/if}
			</form>
			{#if tokenError}
				<p class="max-w-6xl mx-auto mt-2 text-xs text-red-500">{tokenError}</p>
			{/if}
		</div>
	{/if}

	<main class="max-w-6xl mx-auto px-6 py-6">
		{#if children}
			{@render children()}
		{/if}
	</main>
</div>
