import { render } from 'preact';

import { App } from './App';
import { applyThemePreference, readThemePreference } from './lib/theme';
import './styles/index.css';

applyThemePreference(readThemePreference());

const root = document.getElementById('app');
if (root !== null) render(<App />, root);
