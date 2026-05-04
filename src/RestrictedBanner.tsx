type Props = {
  onConvert: () => void;
};

export function RestrictedBanner({ onConvert }: Props) {
  return (
    <div className="restricted-banner" role="status">
      <span className="restricted-banner-msg">
        External markdown file. Recording and AI features are off until you turn it into a Margin
        note.
      </span>
      <div className="restricted-banner-actions">
        <button className="restricted-primary" onClick={onConvert}>
          Turn into Margin note
        </button>
      </div>
    </div>
  );
}
