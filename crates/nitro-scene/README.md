# nitro-scene

The server's retained scene graph: a tree of primitive nodes grouped into
windows, with exact damage tracking, a flat paint list and hit testing.

Pure data plus bookkeeping. No I/O, no rasterization, no threads, no
dependency on the wire protocol — `nitro-core` is the only dependency. The
server owns the socket and translates client-allocated ids into the keys
minted here; the rasterizer consumes a `Vec<PaintItem>` and never touches the
scene at all.

Everything in this crate exists to make one sentence true: **work is
proportional to what changed, not to what is on screen.**

## Data model

```
Scene
├── outputs: Vec<Output>          OutputId, device rect, scale, z-order per layer
├── windows: Arena<WindowKey>     root node + title, layer, size, output, position
├── nodes:   Arena<NodeKey>       the tree
└── buffers: Arena<BufferKey>     client pixels, owned by the scene
```

### Keys and arenas

Every handle is a generational key: a `u32` slot index plus the `u32`
generation that slot had when the key was minted. Removing a value bumps the
slot's generation, so every key handed out before the removal is rejected from
then on with `Error::StaleKey` — never a panic, never a silent hit on a
recycled slot, never UB. A slot whose generation would overflow is retired
instead of being recycled, so an ancient key can never come back to life.

`NodeKey`, `WindowKey` and `BufferKey` are separate types over the same
mechanism; they cannot be mixed up.

### Windows

A window is a **separate table entry**, not a node kind: a `Window` holds the
metadata the shell needs (title, layer, requested size, output, position) plus
the `NodeKey` of its root, which is an ordinary `Group`. That split keeps
`Node` uniform — the traversals never branch on "is this a window?" — while
letting the shell list and restack windows without touching the tree.

A window's root node cannot be reparented or destroyed on its own
(`Error::RootNode`); it lives and dies with the window.

### Nodes

| kind      | payload                                     |
|-----------|---------------------------------------------|
| `Group`   | transform, clip (both apply to children)    |
| `Rect`    | fill, corner radius, border                 |
| `Image`   | buffer key + source rect                    |
| `Text`    | reserved — stores nothing but the kind      |
| `Surface` | reserved — stores nothing but the kind      |

The reserved kinds are accepted, stored and traversed, but produce no paint
item and no damage. They grow a payload when the rasterizer needs text runs and
the Wayland adapter needs external buffers.

Every node carries the common properties (`bounds`, `opacity`, `visible`), its
`ClientId` for ownership checks, its parent and its ordered children. Children
are stored **back to front**: later children paint on top and win hit tests,
and `before: None` means "topmost".

`transform` and `clip` are `Group`-only; the fill properties are `Rect`-only;
`image` is `Image`-only. Setting one on the wrong kind is `Error::WrongKind`
rather than a silently ignored write.

### Coordinate spaces

Client-facing properties are **logical**: `f32`, relative to the parent. A
node's `bounds` positions it in its parent's space; a group's `transform`
applies to its children, about the group's own origin.

Everything cached and everything emitted — world bounds, clips, damage rects,
hit-test input — is in **device pixels**: `i32`, global across all outputs.

The conversion happens exactly once, in the window root's transform:

```
root_transform = translate(output.rect.origin + window.position * output.scale)
               ∘ scale(output.scale)
```

So a node's cached `world_transform` already contains the output's scale and
origin, and the rasterizer never needs to know either. A 2× output simply makes
every device rect twice as large; nothing else in the crate changes. An output
at `x = 1920` simply offsets every window on it.

A window with no output is *unplaced*: it is not painted, cannot be hit, and
produces no damage. Placement is a server decision, so `place_window` takes no
`ClientId`.

## Dirty flags and damage

### Marking

Each mutation sets the narrowest flag that describes it:

| flag        | set by                                  | forces descendants to be revisited |
|-------------|-----------------------------------------|------------------------------------|
| `TRANSFORM` | `set_transform`, window placement       | yes                                |
| `BOUNDS`    | `set_bounds`, window resize             | yes                                |
| `INHERIT`   | `set_opacity`, `set_visible`, `set_clip`| yes                                |
| `PAINT`     | fill, radius, border, image, buffer data| no                                 |
| `STRUCTURE` | a child added or removed                | no                                 |
| `SUBTREE`   | the scene, on every ancestor            | —                                  |

Marking a node also lights a trail of `SUBTREE` flags from it up to its window
root, and puts that root on a small dirty list. The walk up stops as soon as it
meets a node that is already lit, so marking *n* nodes on one path costs one
walk, not *n*.

Setting a property to the value it already has is a no-op: it marks nothing, so
a client that re-sends its whole state every frame still costs nothing.

### The update pass

`Scene::update(&mut DamageSink)` walks only the dirty roots' trails. For each
node it recomputes the world transform, the inherited clip, the accumulated
opacity and visibility, and the device-pixel bounds — top-down, parent before
child. A child with no flags at all is skipped together with its whole subtree
unless an ancestor's `TRANSFORM`/`BOUNDS`/`INHERIT` forces it.

The damage rule is **old ∪ new**:

> For every node whose footprint, visibility or appearance changed, damage both
> the rectangle it used to occupy and the one it occupies now. The first
> repaints whatever was behind it; the second draws it where it is now.

Both rectangles are already narrowed by the clips in force, so a clip group
bounds the damage of everything inside it — however far its children stick out.

"Changed" is decided by comparing the node's **cached world state** against what
the walk just computed — device bounds, whether it paints at all, accumulated
opacity, inherited visibility, and the world transform — *not* by consulting
the node's own dirty flags. That distinction is load-bearing: a descendant
dragged along by an ancestor's opacity, visibility or transform change carries
no flags of its own, so a flag-based test misses it. It also catches the cases
where the bounding box is unchanged but the pixels are not — fading a group,
rotating a square through 90°, mirroring a gradient.

That covers every mutation whose old rectangle is still reachable when `update`
runs. The mutations that *destroy* the old rectangle before then — `destroy_node`,
`reparent`, unplacing a window, restacking, moving an output — bank it in a
pending list at mutation time, and `update` flushes that list first. This is
why no node needs to damage its subtree's old extent: every departed rectangle
was already accounted for by whoever removed it.

Consequences worth stating:

- A node that merely *contains* something that changed adds no damage of its
  own. Its children account for themselves.
- Changes inside an invisible subtree damage nothing; revealing it damages its
  new state, not its old one.
- An update with nothing dirty visits zero nodes and adds zero rects.
- Damage is accumulated into the caller's `Damage` regions and never cleared by
  the scene, so several updates can pile up before a frame is drawn.
- Every rect is clipped to the owning output's device rect on the way in, so a
  region never contains pixels that output cannot draw. This is what keeps a
  window moving *between* outputs honest: the rectangle it vacated is damaged
  on the output it left, the new one on the output it arrived on.
- `DamageSink` only carries the outputs the caller passed. Damage for an output
  that is not in the sink is dropped, which is what the server wants when it is
  redrawing one screen — the scene still ends up clean either way.

`update` also returns a `Configure` for every window whose size changed, which
the server forwards to the owning client.

### Cost

`UpdateStats::visited_nodes` is not a debug aid; it is the contract, and the
stress tests assert on it. In a tree of 11 001 nodes:

| change                          | nodes visited |
|---------------------------------|---------------|
| nothing                         | 0             |
| one leaf's fill                 | 3             |
| ten scattered leaves            | 21            |
| one group's transform           | 12            |
| every leaf                      | 11 001        |

## Traversals

Both traversals read the cached world state, so they must run after `update`.
Neither mutates the scene; both take `&self`.

**`paint_list(output, clip, out)`** appends `PaintItem`s in painter's order:
layers back to front, windows back to front within a layer, children in order.
A subtree whose cached extent misses `clip` is skipped without being walked, as
are invisible and fully transparent subtrees. Each item carries its local→device
transform, its device clip (the node's inherited clip ∩ the requested region),
its accumulated opacity, and its clipped device bounds — everything the
rasterizer needs, with no pointer back into the tree. `out` is appended to,
never cleared.

`PaintItem::opaque_cover()` is the occlusion hint: the device rect an item is
*guaranteed* to fill with fully opaque pixels, so a rasterizer can skip what is
behind it. It is deliberately conservative, because only under-reporting is
sound here — over-reporting would let a caller drop content that is in fact
visible. An item qualifies only when it is a fully opaque, square-cornered,
axis-aligned solid rect *whose device rect lands on exact pixel boundaries*:
`bounds` is rounded outward, so a rect on a half-pixel edge covers its boundary
pixels only partially and reports `None`. Rounded corners, a translucent fill
or border, accumulated opacity below 1.0 and rotation all disqualify it too.
Images never qualify: the scene cannot see their alpha.

**`hit_test(output, point)`** returns the topmost window, deepest node, and the
point in that node's local coordinates (via `Transform::invert`, so rotation and
scale are handled exactly). Windows are tried front to back, children front to
back. Clip groups reject points outside their clip, invisible and fully
transparent subtrees are skipped, and a node only counts as hit if it actually
paints something — so an empty group never swallows a click, and a rotated rect
is not hit in the corners of its bounding box.

`windows(output)` iterates z-order back to front; `windows_front_to_back` is the
reverse; `window_info(key)` returns the metadata.

Both traversals treat a z-order entry that no longer resolves the same way —
they skip it rather than abandoning the walk. The invariant should make that
unreachable; the point is that the two agree if it ever is not.

## Ownership and errors

Every mutation takes a `ClientId` and returns `Result<(), Error>`. A client may
only touch its own nodes and buffers (`Error::NotOwner`); `ClientId::SERVER` may
touch anything, which is how the shell moves and restacks other clients'
windows.

| error           | meaning                                                  |
|-----------------|----------------------------------------------------------|
| `StaleKey`      | the key names an empty or recycled slot                   |
| `NotOwner`      | the client does not own that node or buffer               |
| `WrongKind`     | the property does not exist on that node kind             |
| `BadParent`     | the new parent is the node itself or one of its children  |
| `RootNode`      | the node is a window's root                               |
| `BadSibling`    | `before` is not a child of the parent                     |
| `TooDeep`       | the tree would exceed `MAX_DEPTH` (128)                   |
| `BadBuffer`     | degenerate description, short data, or a bad source rect  |
| `UnknownOutput` | no output with that id                                    |

A refused mutation changes nothing: the cycle check, the depth check and the
buffer checks all run before any write.

`MAX_DEPTH` bounds the recursive traversals to a normal stack and bounds the
cost of a reparent. The mutation-side walks (destroy, reparent, rewrite) use an
explicit scratch stack and do not recurse at all.

## Buffers

`create_buffer` takes ownership of a copy of the client's pixels; the scene
holds them until `destroy_buffer`. The format is an opaque fourcc — the scene
never looks inside a pixel, it only checks that the described bytes exist.

`buffer_mut` hands out `&mut [u8]` for in-place updates. Writing there changes
nothing on screen until `buffer_damaged(key, rects)` says which pixels moved;
that marks every `Image` node whose source rect overlaps, and nothing else. The
scene keeps a buffer→users index, so this costs O(users), not a tree walk.

`destroy_buffer` empties every `Image` node referencing it and marks them dirty,
so the pixels disappear cleanly instead of dangling. Destroying an image *node*
leaves its buffer alone.

## Testing

108 tests, all pure and fast (no I/O, no sleeps, no globals):

- `tests/tree.rs` — shape, and every error path.
- `tests/damage.rs` — damage exactness, clips, outputs, scale, configures.
- `tests/traverse.rs` — paint order, culling, hit testing.
- `tests/buffers.rs` — buffer ownership and damage propagation.
- `tests/stress.rs` — 10 000+ nodes; asserts `visited_nodes` stays proportional.

`just fmt build test` covers it.
