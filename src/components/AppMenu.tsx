type AppMenuProps = {
  onOpenSettings: () => void;
};

/**
 * Hamburger control — opens settings directly (no intermediate menu).
 */
export function AppMenu({ onOpenSettings }: AppMenuProps) {
  return (
    <button
      type="button"
      className="menu-trigger"
      aria-label="settings"
      title="settings"
      onClick={onOpenSettings}
    >
      <span className="menu-bars" aria-hidden="true">
        <span />
        <span />
        <span />
      </span>
    </button>
  );
}
