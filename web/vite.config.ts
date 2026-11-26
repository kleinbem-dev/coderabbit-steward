import { defineWebConfig } from '@kleinbem/web-config/vite';

export default defineWebConfig({
	server: {
		proxy: {
			'/api': 'http://127.0.0.1:8484',
			'/health': 'http://127.0.0.1:8484'
		}
	}
});
