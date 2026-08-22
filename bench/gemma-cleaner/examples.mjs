// Two example reels from the README, represented as the *output* of the local
// extraction pipeline (src/media.rs::extract_social_evidence) would produce.
//
// The full pipeline (yt-dlp -> ffmpeg -> faster-whisper -> tesseract) is not
// installed in this environment, and the user asked us to "mock the real
// codeflow". So we fabricate realistic raw evidence that mimics what Whisper
// and Tesseract actually return for these cooking reels: spoken numbers as
// words, social filler ("like and subscribe", "link in bio"), hashtags, and
// on-screen ingredient amounts. The cleaner (Gemma 4 via OpenRouter) is then
// run for real against this evidence.

export const EXAMPLES = [
  {
    id: "fb-reel-2921942621481069",
    label: "Facebook Reel — 3-Ingredient Fluffy Pancakes",
    url: "https://www.facebook.com/reel/2921942621481069",
    evidence: {
      source_url: "https://www.facebook.com/reel/2921942621481069",
      title: "3-Ingredient Fluffy Pancakes 🥞 | 10 min breakfast",
      description:
        "The easiest fluffy pancakes you'll ever make! Just blend and cook 🤤 Full written recipe is on my blog, link in bio 👇 Don't forget to SAVE this for Sunday brunch! Follow @bailees.kitchen for more easy recipes every week ✨ #pancakes #breakfast #easyrecipe #mealprep #healthyeating #foodtok",
      duration_seconds: 42,
      audio_transcript:
        "hey guys welcome back to my channel if you are new here make sure to hit that like button and subscribe so you never miss a recipe okay today we are making three ingredient fluffy pancakes and they are so good so you are gonna take two ripe bananas and one egg and about two table spoons of peanut butter and you just blend them together until smooth then heat a nonstick pan on medium and pour little circles cook for like two to three minutes each side until they are golden and that is it so fluffy tag me if you make them and check the link in my bio for the printable recipe bye guys",
      ocr: [
        { timestamp_seconds: 4, text: "2 bananas" },
        { timestamp_seconds: 8, text: "1 egg" },
        { timestamp_seconds: 12, text: "2 tbsp peanut butter" },
        { timestamp_seconds: 28, text: "cook 2-3 min" },
        { timestamp_seconds: 30, text: "medium heat" },
      ],
      warnings: [],
      cleaned_recipe_text: "",
    },
  },
  {
    id: "ig-post-DZNQT3Pt3Ja",
    label: "Instagram Post — Creamy Tuscan Chicken Pasta",
    url: "https://www.instagram.com/p/DZNQT3Pt3Ja/",
    evidence: {
      source_url: "https://www.instagram.com/p/DZNQT3Pt3Ja/",
      title: "Creamy Tuscan Chicken Pasta 🍅🥬 (20 min!)",
      description:
        "Save this for dinner!! Creamy Tuscan chicken pasta that tastes like a restaurant 🤌 full recipe linked in my story + bio. Follow for weekly pasta nights 💕 #pastarecipe #dinnerideas #tuscany #comfortfood #weeknightdinner #instafood",
      duration_seconds: 55,
      audio_transcript:
        "what's up everyone welcome back today we are making creamy tuscan chicken pasta and it is unreal so first you want to season two chicken breasts with salt pepper and garlic powder and sear them in a skillet with a little olive oil for about four minutes each side then take them out and in the same pan add like one table spoon of butter and three cloves of garlic and half a cup of sun dried tomatoes cook one minute then pour in one cup of heavy cream and half a cup of parmesan and let it simmer for three minutes until thick stir in two handfuls of spinach and the chicken and serve over pasta that's it guys don't forget to like and follow",
      ocr: [
        { timestamp_seconds: 6, text: "2 chicken breasts" },
        { timestamp_seconds: 10, text: "1 tbsp butter" },
        { timestamp_seconds: 14, text: "3 cloves garlic" },
        { timestamp_seconds: 18, text: "1/2 cup sun dried tomatoes" },
        { timestamp_seconds: 24, text: "1 cup heavy cream" },
        { timestamp_seconds: 28, text: "1/2 cup parmesan" },
        { timestamp_seconds: 34, text: "2 handfuls spinach" },
      ],
      warnings: [],
      cleaned_recipe_text: "",
    },
  },
];
