// The app: a list of notes in a database inside it, kept across launches.
// What it runs is in Notes.swift — the same SQL a server would take.

import SwiftUI

@main
struct NotesApp: App {
    @StateObject private var notes = Notes()

    var body: some Scene {
        WindowGroup {
            NavigationStack {
                List {
                    Section {
                        HStack {
                            TextField("A new note", text: $notes.draft)
                                .onSubmit { notes.add() }
                            Button("Add") { notes.add() }
                                .disabled(notes.draft.isEmpty)
                        }
                    }
                    Section(notes.summary) {
                        ForEach(notes.items, id: \.id) { note in
                            VStack(alignment: .leading) {
                                Text(note.body)
                                Text(note.created).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
                .searchable(text: $notes.query)
                .onChange(of: notes.query) { _ in notes.load() }
                .navigationTitle("Notes")
            }
        }
    }
}
