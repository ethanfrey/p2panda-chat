# Chat Idea

A fully p2p Tauri app as an alternative to rocket.chat or discord.

Start with Loro data models, persistence, auth - groups and spaces.

Auth looks like:

Top-Group-ID: [User-A-Group-ID, User-B-Group-ID, ..,]
User-A-Group-ID: [Device-A-1-ID, Device-A-2-ID, ..,]

## Model V1

Basic group chat - text, reply, reactions, threads

- Profile
    - owned by one user
    - valid for their subgroup / all devices)
- Channel
    - create -> CHAN-ID
    - rename (owner or any write?)
    - no delete -> reversible "archive")
    - moveable list of channels to browse
- Messages
    - create -> MSG-ID, link to author, CHAN-ID (note event-id)
    - edit -> author subgroup can modify, note changed event-id, timestamp of edit
    - display messages filtered by channel sorted by most recent message at bottom
    - reply-to (MSG-ID, event) -> shows if replied msg changed
- Reactions
    - create -> REACTION-ID, link to author, MSG-ID, msg-event-id
    - delete by author
- Threads
    - create -> THREAD-ID, anchor to MSG-ID where it starts (attach like reaction)
    - any message may include non-nil thread id, it will be shown in that thread

- Real-time updates (when event comes in and processed, go to UI)


### Questions

Ordering:

- Use timestamps for ordering?
- Causal ordering (reply depends on previous, always after)
- Both -> Reference "show after message" (in channel or in thread) to go below them. Parallel messages are sorted by timestamp. This "show after" is maxed with reply-to message, another dependency
- Never use timestamps?? Maybe different ordering for different people who sync differently, but any part of interleaved communication shows up properly referenced (people talking back and forth)

## Model V2

Private Spaces (maybe full spaces only comes in here and v1 just restricts write and uses "bearer token" topic ID to control read)

- Related topics
    - Topic ID = Hash(Init Data), so you can find / create knowing this. Initial Data:
        - Parent Topic ID (If two people want to DM as part of one chat or a different one, group-ID references data from parent log)
        - Initial Group Members (User-A-Group-ID / Write, User-B-Group-ID / Write)
        - Name (if we want multiple topics, but use a well-known name for common tasks, eg. "DM")
    - Associate space with this group
    - Cross-link to data structures from main group, state of User-A-Group-ID comes from there

1. Use for private messaging:

Example: We can pre-compute virtual DM between us and anyone else. Just calculate initial data for the two, using "DM" name. Subscribe and see if any data. First event must be the initial data (this is like the deterministic address for contract deployment). Note: others cannot read data but can see you have communicated.

Off-the-record: Create initial message with non-standard name (name = "top-secret-code") and start topic ID from it. Send DM to user A asking to use a new channel, no one else can see.

Group Chat: Make group with multiple users, send the info via DM to multiple people, all can listen

Why is this part of the app??
- Sharing group-ids -> Maybe this can be a much more generic thing
- Ideally "sub discussions" so they show there. Not really a person-to-person messenger, but community focused one-on-one discussion

2. Use to manage private notes ("Personal Area")

- Name is "notes" (or something non-standard), only one user-group included
- You can add private comments that reference parent topic data structures
    - Labels on messages (important, spam, etc) just for you
    - One device (using on-device AI) could monitor and auto-tag and then this updates all my UI, but no one else sees
- Store some comments, quick notepad in the app

### Questions

Cross app stuff: once we start with "related topics" do these even have to be part of the same app? What do we really want to do here?

- Maintain my user-device-group in one place, can use that in multiple apps
    - Maybe I want to grant a device read all and write on one app... but do I need completely different groups? Could it merge the "source group" and a local diff?
- Link to a new topic with new app to view it
    - Space for my personal notes (hack.md) linked from my comms app.
    - Maybe I could just DM myself a link, link being a special URL that launches and app and has the topic in it to start up if app installed? (mobile style) Other ideas?


## Model V3

Polish

- Search over messages (fully local, how to store)
- Improve app state materialziation
- Snapshots??
- Files / Photos (blobs) -> TODO
- Separate tab to show all links in a channel, all photos, etc
- Notifications (system notification if in the background)


## Side-app

* Hack.md document app
* Groups App (shared by both, where you add/remove devices) - share public topic so ayone can read, no spaces here
    * Key here -> It needs the same identifier over all apps, store data by (keyname / app name) dir

Topic + Schema => identifier
- Can register local apps for handling a schema, one of my phone, another on my laptop, another on my home-server (headless processing)