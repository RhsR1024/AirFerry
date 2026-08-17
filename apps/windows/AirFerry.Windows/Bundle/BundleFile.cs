namespace AirFerry.Windows.Bundle;

/// <summary>
/// One recovered multi-entry file (AF2 Manifest bundle member): its wire path
/// and raw content slice. Successor of the v1 BundleParser member type —
/// the parser is gone (F2 v1-artifact removal), the recovered-file pipeline
/// (continuous save / share export) still flows through this shape.
/// </summary>
public sealed record BundleFile(string Name, byte[] Data);
